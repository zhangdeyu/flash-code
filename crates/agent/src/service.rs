//! Async Runtime control plane: command bus, event envelope, and `RunHandle`.
//!
//! `start()` consumes an `AgentRuntime<P>` and spawns the run loop via
//! `tokio::spawn`. The generic `P` is captured inside the spawned future; the
//! returned `RunHandle` is non-generic (the `JoinHandle` erases `P`).

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use flash_core::{create_session_with_limits_async, CancellationToken, Event};
use flash_provider::ChatProvider;

use crate::error::{AgentError, AgentRun};
use crate::hooks::{EventObserver, RejectingApproval};
use crate::runtime::AgentRuntime;

/// Commands that can be sent to a running session via the command bus.
///
/// `Cancel` is wired in this subtask. `Approve`, `Steer`, and `FollowUp` are
/// defined as part of the public API but are deferred to A0.3c/A4.
#[derive(Debug, Clone)]
pub enum RuntimeCommand {
    /// Cancel the running session. The run loop exits cleanly without
    /// committing a partial assistant message.
    Cancel,
    /// Resolve a pending approval request. Deferred to A0.3c.
    Approve {
        call_id: String,
        decision: ApprovalDecision,
    },
    /// Inject a steering message into the current run. Deferred to A4.
    Steer { message: String },
    /// Queue a follow-up message after the current run completes. Deferred to A4.
    FollowUp { message: String },
}

/// The outcome of an approval request. Used by `RuntimeCommand::Approve`.
///
/// Deferred to A3.3; defined here so `RuntimeCommand` is complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    AllowOnce,
    AllowForSession,
    AllowAlways,
    Deny,
}

/// A runtime event wrapped with session metadata and a monotonic sequence.
#[derive(Debug, Clone)]
pub struct EventEnvelope {
    pub session_id: String,
    pub sequence: u64,
    pub turn_id: Option<String>,
    pub request_id: Option<String>,
    pub event: Event,
}

/// Handle to a running session. Non-generic: the `JoinHandle` erases `P`.
pub struct RunHandle {
    pub session_id: String,
    pub commands: tokio::sync::mpsc::Sender<RuntimeCommand>,
    pub events: tokio::sync::mpsc::Receiver<EventEnvelope>,
    pub completion: tokio::task::JoinHandle<Result<AgentRun, AgentError>>,
}

/// `EventObserver` adapter that wraps each `Event` in an `EventEnvelope` and
/// sends it on the bounded events channel. Uses `try_send` so a slow consumer
/// never blocks the run loop; events are also persisted via `append_event_async`
/// in `emit_event`, so dropping live events does not affect durability.
struct ChannelObserver {
    session_id: String,
    sequence: AtomicU64,
    events_tx: tokio::sync::mpsc::Sender<EventEnvelope>,
}

impl EventObserver for ChannelObserver {
    fn on_event(&mut self, event: &Event) {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let envelope = EventEnvelope {
            session_id: self.session_id.clone(),
            sequence,
            turn_id: None,
            request_id: None,
            event: event.clone(),
        };
        let _ = self.events_tx.try_send(envelope);
    }
}

impl<P> AgentRuntime<P>
where
    P: ChatProvider + Send + Sync + 'static,
{
    /// Consume the runtime and spawn the run loop as a detached task.
    ///
    /// Creates the session, sets up bounded command (64) and event (256)
    /// channels, spawns the run loop with a cancellation bridge, and returns a
    /// non-generic `RunHandle`.
    pub async fn start(
        mut self,
        workspace_root: &Path,
        task: &str,
    ) -> Result<RunHandle, AgentError> {
        let session =
            create_session_with_limits_async(workspace_root.to_path_buf(), self.storage_limits)
                .await?;
        let session_id = session.id.clone();

        let (events_tx, events_rx) = tokio::sync::mpsc::channel::<EventEnvelope>(256);
        let (commands_tx, mut commands_rx) = tokio::sync::mpsc::channel::<RuntimeCommand>(64);

        let cancel_token = CancellationToken::new();

        // Spawn a command handler that cancels the run on `Cancel`.
        // `Approve`/`Steer`/`FollowUp` are received but not yet wired.
        let cancel_for_cmd = cancel_token.clone();
        tokio::spawn(async move {
            while let Some(cmd) = commands_rx.recv().await {
                match cmd {
                    RuntimeCommand::Cancel => cancel_for_cmd.cancel(),
                    RuntimeCommand::Approve { .. }
                    | RuntimeCommand::Steer { .. }
                    | RuntimeCommand::FollowUp { .. } => {
                        // Deferred to A0.3c/A4.
                    }
                }
            }
        });

        // Spawn the run loop. The generic `P` is captured here; the
        // `JoinHandle` erases it so `RunHandle` stays non-generic.
        let workspace_root = workspace_root.to_path_buf();
        let task = task.to_string();
        let cancel_for_should_cancel = cancel_token.clone();
        let observer_session_id = session_id.clone();
        let completion = tokio::spawn(async move {
            let mut observer = ChannelObserver {
                session_id: observer_session_id,
                sequence: AtomicU64::new(0),
                events_tx,
            };
            let should_cancel = move || cancel_for_should_cancel.is_cancelled();
            let mut approval = RejectingApproval;
            self.run_turns(
                &session,
                None,
                &workspace_root,
                &task,
                &mut observer,
                cancel_token,
                should_cancel,
                &mut approval,
            )
            .await
        });

        Ok(RunHandle {
            session_id,
            commands: commands_tx,
            events: events_rx,
            completion,
        })
    }

    /// Headless run: start the session, drain events (ignored), and await
    /// completion. This is the thin wrapper used by the CLI `run` command.
    pub async fn run_task(self, workspace_root: &Path, task: &str) -> Result<AgentRun, AgentError> {
        let mut handle = self.start(workspace_root, task).await?;
        // Drain events; the headless path ignores them.
        while handle.events.recv().await.is_some() {}
        handle
            .completion
            .await
            .map_err(|join_error| AgentError::Finalization {
                primary: format!("run task panicked: {join_error}"),
                finalize: "join error".to_string(),
            })?
    }
}
