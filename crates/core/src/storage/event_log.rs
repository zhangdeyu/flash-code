use std::fs;
use std::path::Path;

use crate::protocol::Event;

use super::session::touch_session;
use super::session::Session;
use super::{
    append_line, run_blocking_storage, timestamp, EventRecord, StorageError, StorageLimits,
};

pub fn append_event(session: &Session, event: Event) -> Result<(), StorageError> {
    const ERROR_RESERVE_BYTES: u64 = 512;
    const TERMINAL_RESERVE_BYTES: u64 = 1024;
    let path = session.path.join("events.jsonl");
    let mut sequence = session
        .sequence
        .lock()
        .map_err(|_| StorageError::SequenceLock)?;
    let line = event_to_jsonl(*sequence, &session.id, &event)?;
    if line.len().saturating_add(1) > session.limits.max_event_bytes {
        return Err(StorageError::ResourceLimit {
            resource: "event record".to_string(),
            limit: session.limits.max_event_bytes as u64,
        });
    }
    let max_jsonl_bytes = match event {
        Event::SessionFinished { .. } => session.limits.max_jsonl_bytes,
        Event::Error { .. } => session
            .limits
            .max_jsonl_bytes
            .saturating_sub(ERROR_RESERVE_BYTES),
        _ => session
            .limits
            .max_jsonl_bytes
            .saturating_sub(TERMINAL_RESERVE_BYTES),
    };
    append_line(&path, &line, max_jsonl_bytes)?;
    *sequence += 1;
    touch_session(session)
}

pub async fn append_event_async(session: Session, event: Event) -> Result<(), StorageError> {
    run_blocking_storage(move || append_event(&session, event)).await
}

pub fn replay_events(path: &Path) -> Result<Vec<String>, StorageError> {
    super::recovery::repair_jsonl::<EventRecord>(path, StorageLimits::default().max_jsonl_bytes)?;
    let content = fs::read_to_string(path)?;
    content
        .lines()
        .map(|line| {
            let record: EventRecord = serde_json::from_str(line)?;
            Ok(format!(
                "{}: {}",
                record.sequence,
                record.event.event_type()
            ))
        })
        .collect()
}

/// Serialize an event into a JSONL line.
pub(super) fn event_to_jsonl(
    sequence: u64,
    session_id: &str,
    event: &Event,
) -> Result<String, StorageError> {
    Ok(serde_json::to_string(&EventRecord {
        version: "1".to_string(),
        sequence,
        timestamp: timestamp(),
        session_id: session_id.to_string(),
        event: event.clone(),
    })?)
}

/// Compute the next sequence number by counting existing lines.
pub(super) fn next_sequence(path: &Path) -> Result<u64, StorageError> {
    if !path.exists() {
        return Ok(1);
    }
    let content = fs::read_to_string(path)?;
    Ok(content.lines().count() as u64 + 1)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::protocol::{Event, Outcome, SessionStatus, ToolResultStatus};

    #[test]
    fn replay_events_should_return_event_timeline() {
        let root = super::super::test_support::temp_dir("replay");
        fs::create_dir_all(&root).unwrap();
        let session = super::super::session::create_session(&root).unwrap();

        let timeline = replay_events(&session.path.join("events.jsonl")).unwrap();

        assert_eq!(timeline, vec!["1: session_started"]);
    }

    #[test]
    fn event_schema_should_match_golden() {
        let root = super::super::test_support::temp_dir("event_schema_golden");
        let session = super::super::session::test_session(&root, "session_golden");

        append_event(
            &session,
            Event::SessionStarted {
                session_id: "session_golden".to_string(),
            },
        )
        .unwrap();
        append_event(
            &session,
            Event::UserMessageAppended {
                message_id: "msg_user".to_string(),
            },
        )
        .unwrap();
        append_event(
            &session,
            Event::ModelRequestStarted {
                request_id: "req_1".to_string(),
                attempt: 1,
                model: "deepseek-test".to_string(),
            },
        )
        .unwrap();
        append_event(
            &session,
            Event::ReasoningDelta {
                request_id: "req_1".to_string(),
                attempt: 1,
                text: "think\nstep".to_string(),
            },
        )
        .unwrap();
        append_event(
            &session,
            Event::AssistantDelta {
                request_id: "req_1".to_string(),
                attempt: 1,
                text: "hello".to_string(),
            },
        )
        .unwrap();
        append_event(
            &session,
            Event::ModelAttemptFailed {
                request_id: "req_1".to_string(),
                attempt: 1,
                retryable: true,
                message: "retry".to_string(),
            },
        )
        .unwrap();
        append_event(
            &session,
            Event::ModelAttemptCommitted {
                request_id: "req_1".to_string(),
                attempt: 2,
            },
        )
        .unwrap();
        append_event(
            &session,
            Event::AssistantMessageCompleted {
                message_id: "msg_assistant".to_string(),
            },
        )
        .unwrap();
        append_event(
            &session,
            Event::ToolCallRequested {
                call_id: "call_1".to_string(),
                name: "Read".to_string(),
            },
        )
        .unwrap();
        append_event(
            &session,
            Event::ApprovalRequired {
                call_id: "call_1".to_string(),
            },
        )
        .unwrap();
        append_event(
            &session,
            Event::ApprovalResolved {
                call_id: "call_1".to_string(),
                approved: true,
            },
        )
        .unwrap();
        append_event(
            &session,
            Event::ToolStarted {
                call_id: "call_1".to_string(),
                name: "Read".to_string(),
            },
        )
        .unwrap();
        append_event(
            &session,
            Event::ToolOutputDelta {
                call_id: "call_1".to_string(),
                stream: "stdout".to_string(),
                text: "file text".to_string(),
            },
        )
        .unwrap();
        append_event(
            &session,
            Event::ToolFinished {
                call_id: "call_1".to_string(),
                status: ToolResultStatus::Success,
            },
        )
        .unwrap();
        append_event(
            &session,
            Event::UsageRecorded {
                request_id: "req_1".to_string(),
                attempt: 2,
                input_tokens: 11,
                output_tokens: 22,
            },
        )
        .unwrap();
        append_event(
            &session,
            Event::Error {
                message: "boom".to_string(),
            },
        )
        .unwrap();
        append_event(
            &session,
            Event::SessionFinished {
                outcome: crate::protocol::Outcome::Succeeded,
            },
        )
        .unwrap();

        let actual = fs::read_to_string(session.path.join("events.jsonl")).unwrap();

        assert_eq!(
            super::super::test_support::normalize_json_string_field(
                &actual,
                "timestamp",
                "<timestamp>"
            ),
            include_str!("../../tests/golden/events_v1.jsonl")
        );
    }

    #[test]
    fn streaming_events_should_not_commit_interrupted_assistant_message() {
        let root = super::super::test_support::temp_dir("interrupted_turn");
        fs::create_dir_all(&root).unwrap();
        let session = super::super::session::create_session(&root).unwrap();

        append_event(
            &session,
            Event::AssistantDelta {
                request_id: "req_1".to_string(),
                attempt: 1,
                text: "partial answer".to_string(),
            },
        )
        .unwrap();
        append_event(
            &session,
            Event::Error {
                message: "cancelled".to_string(),
            },
        )
        .unwrap();

        let messages = fs::read_to_string(session.path.join("messages.jsonl")).unwrap();

        assert!(messages.is_empty());
    }

    #[test]
    fn event_and_jsonl_limits_should_return_structured_errors() {
        let root = super::super::test_support::temp_dir("storage_limits");
        fs::create_dir_all(&root).unwrap();
        let mut session = super::super::session::create_session(&root).unwrap();
        session.limits = StorageLimits {
            max_event_bytes: 256,
            max_jsonl_bytes: 4096,
        };

        let event_error = append_event(
            &session,
            Event::AssistantDelta {
                request_id: "request".to_string(),
                attempt: 1,
                text: "x".repeat(1024),
            },
        )
        .unwrap_err();
        session.limits.max_event_bytes = 1024;
        session.limits.max_jsonl_bytes = fs::metadata(session.path.join("events.jsonl"))
            .unwrap()
            .len()
            + 512;
        let jsonl_error = append_event(
            &session,
            Event::AssistantDelta {
                request_id: "request".to_string(),
                attempt: 1,
                text: "y".repeat(32),
            },
        )
        .unwrap_err();

        assert!(matches!(
            event_error,
            StorageError::ResourceLimit { resource, .. } if resource == "event record"
        ));
        assert!(matches!(jsonl_error, StorageError::ResourceLimit { .. }));
    }

    #[test]
    fn jsonl_limit_should_reserve_space_for_terminal_event() {
        let root = super::super::test_support::temp_dir("terminal_reserve");
        fs::create_dir_all(&root).unwrap();
        let mut session = super::super::session::create_session(&root).unwrap();
        let current = fs::metadata(session.path.join("events.jsonl"))
            .unwrap()
            .len();
        session.limits = StorageLimits {
            max_event_bytes: 1024,
            max_jsonl_bytes: current + 512,
        };

        let normal = append_event(
            &session,
            Event::AssistantDelta {
                request_id: "request".to_string(),
                attempt: 1,
                text: "blocked".to_string(),
            },
        )
        .unwrap_err();
        append_event(
            &session,
            Event::SessionFinished {
                outcome: Outcome::Failed,
            },
        )
        .unwrap();
        super::super::session::finalize_session(&session, Outcome::Failed).unwrap();

        assert!(matches!(normal, StorageError::ResourceLimit { .. }));
        let loaded = super::super::session::load_session(&root, &session.id).unwrap();
        assert_eq!(loaded.status, SessionStatus::Failed);
        assert_eq!(
            fs::read_to_string(session.path.join("events.jsonl"))
                .unwrap()
                .matches("\"type\":\"session_finished\"")
                .count(),
            1
        );
    }
}
