use std::time::Duration;

use flash_core::Event;
use flash_provider::{ChatRequest, ProviderError, ProviderEvent};

use crate::error::AgentError;
use crate::finalize::emit_event;
use crate::hooks::EventObserver;
use crate::runtime::AgentRuntime;

impl<P> AgentRuntime<P>
where
    P: flash_provider::ChatProvider,
{
    pub(crate) async fn chat_with_retry_streaming<O: EventObserver>(
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
                        let delay = retry_delay(request_id, attempt, error.retry_after());
                        tokio::select! {
                            () = request.cancellation.cancelled() => {
                                return Err(AgentError::Provider(ProviderError::Cancelled(
                                    "provider retry cancelled".to_string(),
                                )));
                            }
                            () = tokio::time::sleep(delay) => {}
                        }
                        continue;
                    }
                    return Err(AgentError::Provider(error));
                }
            }
        }
    }
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

fn retry_delay(request_id: &str, attempt: u32, retry_after: Option<Duration>) -> Duration {
    const BASE_MILLIS: u64 = 100;
    const MAX_MILLIS: u64 = 2_000;
    const MAX_SERVER_DELAY: Duration = Duration::from_secs(30);
    let exponent = attempt.saturating_sub(1).min(4);
    let exponential = BASE_MILLIS.saturating_mul(1_u64 << exponent);
    let hash = request_id.bytes().fold(u64::from(attempt), |value, byte| {
        value.wrapping_mul(1_099_511_628_211) ^ u64::from(byte)
    });
    let jitter = hash % (exponential / 2 + 1);
    let policy = Duration::from_millis((exponential + jitter).min(MAX_MILLIS));
    retry_after.map_or(policy, |server| server.min(MAX_SERVER_DELAY).max(policy))
}

#[cfg(test)]
mod tests {
    use super::retry_delay;
    use crate::test_support::{
        only_session_path, prepared_workspace, temp_dir, AlwaysErrorProvider, CancellableProvider,
        MaxTokensProvider, PartialProvider, PartialRetryProvider, RetryAfterProvider,
        RetryProvider,
    };
    use crate::{AgentError, AgentOptions, AgentRuntime};
    use flash_core::{CancellationToken, Outcome, PermissionPolicy, ToolRegistry};
    use flash_provider::ProviderError;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn retry_delay_should_be_deterministic_exponential_and_respect_server_minimum() {
        let first = retry_delay("request", 1, None);
        let repeated = retry_delay("request", 1, None);
        let second = retry_delay("request", 2, None);
        let server = retry_delay("request", 1, Some(Duration::from_secs(3)));

        assert_eq!(first, repeated);
        assert!(first >= Duration::from_millis(100));
        assert!(second >= Duration::from_millis(200));
        assert_eq!(server, Duration::from_secs(3));
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
    async fn run_task_should_stop_retrying_server_errors_at_three_attempts() {
        let root = prepared_workspace("server_retry_limit");
        let calls = Arc::new(AtomicU32::new(0));
        let mut runtime = AgentRuntime::new(
            AlwaysErrorProvider {
                error: ProviderError::Server {
                    message: "temporary outage".to_string(),
                    retry_after: None,
                },
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
        let started = tokio::time::Instant::now();

        let error = runtime.run_task(&root, "retry server").await.unwrap_err();

        assert!(matches!(error, AgentError::Provider(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(started.elapsed() >= Duration::from_millis(300));
        let metadata = fs::read_to_string(only_session_path(&root).join("session.json")).unwrap();
        assert!(metadata.contains("\"status\":\"failed\""));
    }

    #[tokio::test]
    async fn run_task_should_finalize_timeout_errors() {
        let root = prepared_workspace("timeout_finalize");
        let calls = Arc::new(AtomicU32::new(0));
        let mut runtime = AgentRuntime::new(
            AlwaysErrorProvider {
                error: ProviderError::Timeout("first byte timed out".to_string()),
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

        let result = runtime.run_task(&root, "timeout").await;

        assert!(matches!(result, Err(AgentError::Provider(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        let session = only_session_path(&root);
        assert!(fs::read_to_string(session.join("session.json"))
            .unwrap()
            .contains("\"status\":\"failed\""));
        assert_eq!(
            fs::read_to_string(session.join("events.jsonl"))
                .unwrap()
                .matches("\"type\":\"session_finished\"")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn run_task_should_not_retry_permanent_http_failures() {
        for (name, error) in [
            (
                "authentication",
                ProviderError::Authentication("bad key".to_string()),
            ),
            ("billing", ProviderError::Billing("no credit".to_string())),
            (
                "invalid_request",
                ProviderError::InvalidRequest("bad request".to_string()),
            ),
        ] {
            let root = prepared_workspace(name);
            let calls = Arc::new(AtomicU32::new(0));
            let mut runtime = AgentRuntime::new(
                AlwaysErrorProvider {
                    error,
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

            let result = runtime.run_task(&root, "do not retry").await;

            assert!(matches!(result, Err(AgentError::Provider(_))), "{name}");
            assert_eq!(calls.load(Ordering::SeqCst), 1, "{name}");
        }
    }

    #[tokio::test]
    async fn retry_after_should_delay_the_next_attempt() {
        let root = prepared_workspace("retry_after");
        let calls = Arc::new(AtomicU32::new(0));
        let mut runtime = AgentRuntime::new(
            RetryAfterProvider {
                calls: Arc::clone(&calls),
                retry_after: Duration::from_millis(450),
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
        let started = tokio::time::Instant::now();

        let run = runtime
            .run_task(&root, "respect retry after")
            .await
            .unwrap();

        assert_eq!(run.outcome, Outcome::Succeeded);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(started.elapsed() >= Duration::from_millis(450));
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
        let mut observer = crate::hooks::NoopObserver;
        let mut approval = crate::hooks::RejectingApproval;

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
}
