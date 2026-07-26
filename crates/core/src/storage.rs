use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::protocol::{
    ContentBlock, Event, Message, Outcome, Role, SessionStatus, ToolResultStatus,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub id: String,
    pub root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub workspace_root: PathBuf,
    pub path: PathBuf,
    sequence: Arc<Mutex<u64>>,
    finalizing: Arc<AtomicBool>,
}

impl PartialEq for Session {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.workspace_root == other.workspace_root
            && self.path == other.path
    }
}

impl Eq for Session {}

impl Session {
    pub fn begin_finalize(&self) -> bool {
        self.finalizing
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }
}

#[derive(Debug)]
pub enum StorageError {
    Io(std::io::Error),
    Parse(String),
    TaskJoin(String),
    SequenceLock,
    WorkspaceMismatch { expected: PathBuf, actual: PathBuf },
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "storage io error: {error}"),
            Self::Parse(message) => write!(formatter, "storage parse error: {message}"),
            Self::TaskJoin(message) => write!(formatter, "storage task join error: {message}"),
            Self::SequenceLock => write!(formatter, "storage event sequence lock is poisoned"),
            Self::WorkspaceMismatch { expected, actual } => write!(
                formatter,
                "session belongs to `{}`, current workspace is `{}`",
                expected.display(),
                actual.display()
            ),
        }
    }
}

impl std::error::Error for StorageError {}

impl From<std::io::Error> for StorageError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for StorageError {
    fn from(error: serde_json::Error) -> Self {
        Self::Parse(error.to_string())
    }
}

pub fn init_workspace(root: &Path) -> Result<Workspace, StorageError> {
    let flash_dir = root.join(".flash");
    let sessions_dir = flash_dir.join("sessions");
    fs::create_dir_all(&sessions_dir)?;
    let workspace = Workspace {
        id: format!("workspace_{}", stable_workspace_id(root)),
        root: root.to_path_buf(),
    };
    let workspace_json = serde_json::to_string(&WorkspaceRecord {
        version: "1".to_string(),
        workspace_id: workspace.id.clone(),
        root: root.display().to_string(),
    })?;
    fs::write(
        flash_dir.join("workspace.json"),
        format!("{workspace_json}\n"),
    )?;
    Ok(workspace)
}

pub fn create_session(root: &Path) -> Result<Session, StorageError> {
    init_workspace(root)?;
    let id = new_id("session");
    let session_dir = root.join(".flash").join("sessions").join(&id);
    fs::create_dir_all(session_dir.join("artifacts"))?;
    let session = Session {
        id: id.clone(),
        workspace_root: root.to_path_buf(),
        path: session_dir.clone(),
        sequence: Arc::new(Mutex::new(1)),
        finalizing: Arc::new(AtomicBool::new(false)),
    };
    let now = timestamp();
    let session_json = serde_json::to_string(&SessionRecord {
        version: "1".to_string(),
        session_id: id.clone(),
        workspace_root: root.display().to_string(),
        created_at: now.clone(),
        updated_at: now,
        status: SessionStatus::Running,
    })?;
    fs::write(
        session_dir.join("session.json"),
        format!("{session_json}\n"),
    )?;
    fs::write(session_dir.join("messages.jsonl"), "")?;
    fs::write(session_dir.join("events.jsonl"), "")?;
    append_event(&session, Event::SessionStarted { session_id: id })?;
    Ok(session)
}

pub async fn create_session_async(root: PathBuf) -> Result<Session, StorageError> {
    run_blocking_storage(move || create_session(&root)).await
}

pub fn append_system_message(session: &Session, text: &str) -> Result<Message, StorageError> {
    let message = Message {
        id: new_id("msg"),
        role: Role::System,
        created_at: timestamp(),
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    };
    append_line(
        &session.path.join("messages.jsonl"),
        &message_to_jsonl(&message)?,
    )?;
    touch_session(session)?;
    Ok(message)
}

pub async fn append_system_message_async(
    session: Session,
    text: String,
) -> Result<Message, StorageError> {
    run_blocking_storage(move || append_system_message(&session, &text)).await
}

pub fn load_session(root: &Path, session_id: &str) -> Result<Session, StorageError> {
    let session_dir = root.join(".flash").join("sessions").join(session_id);
    let content = fs::read_to_string(session_dir.join("session.json"))?;
    let record: SessionRecord = serde_json::from_str(&content)?;
    let expected = PathBuf::from(record.workspace_root);
    if expected != root {
        return Err(StorageError::WorkspaceMismatch {
            expected,
            actual: root.to_path_buf(),
        });
    }
    let sequence = next_sequence(&session_dir.join("events.jsonl"))?;
    let finalized = record.status != SessionStatus::Running;
    Ok(Session {
        id: session_id.to_string(),
        workspace_root: root.to_path_buf(),
        path: session_dir,
        sequence: Arc::new(Mutex::new(sequence)),
        finalizing: Arc::new(AtomicBool::new(finalized)),
    })
}

pub fn append_user_message(session: &Session, text: &str) -> Result<Message, StorageError> {
    let message = Message {
        id: new_id("msg"),
        role: Role::User,
        created_at: timestamp(),
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    };
    append_line(
        &session.path.join("messages.jsonl"),
        &message_to_jsonl(&message)?,
    )?;
    touch_session(session)?;
    append_event(
        session,
        Event::UserMessageAppended {
            message_id: message.id.clone(),
        },
    )?;
    Ok(message)
}

pub async fn append_user_message_async(
    session: Session,
    text: String,
) -> Result<Message, StorageError> {
    run_blocking_storage(move || append_user_message(&session, &text)).await
}

pub fn append_assistant_message(
    session: &Session,
    reasoning: &str,
    text: &str,
    tool_uses: &[(String, String, Value)],
) -> Result<Message, StorageError> {
    let mut content = Vec::new();
    if !reasoning.is_empty() {
        content.push(ContentBlock::Reasoning {
            text: reasoning.to_string(),
        });
    }
    if !text.is_empty() {
        content.push(ContentBlock::Text {
            text: text.to_string(),
        });
    }
    for (call_id, name, input) in tool_uses {
        content.push(ContentBlock::ToolUse {
            call_id: call_id.clone(),
            name: name.clone(),
            input: input.clone(),
        });
    }
    let message = Message {
        id: new_id("msg"),
        role: Role::Assistant,
        created_at: timestamp(),
        content,
    };
    append_line(
        &session.path.join("messages.jsonl"),
        &message_to_jsonl(&message)?,
    )?;
    touch_session(session)?;
    append_event(
        session,
        Event::AssistantMessageCompleted {
            message_id: message.id.clone(),
        },
    )?;
    Ok(message)
}

pub async fn append_assistant_message_async(
    session: Session,
    reasoning: String,
    text: String,
    tool_uses: Vec<(String, String, Value)>,
) -> Result<Message, StorageError> {
    run_blocking_storage(move || append_assistant_message(&session, &reasoning, &text, &tool_uses))
        .await
}

pub fn append_tool_result_message(
    session: &Session,
    call_id: &str,
    status: ToolResultStatus,
    text: &str,
) -> Result<Message, StorageError> {
    let message = Message {
        id: new_id("msg"),
        role: Role::Tool,
        created_at: timestamp(),
        content: vec![
            ContentBlock::ToolResult {
                call_id: call_id.to_string(),
                status,
            },
            ContentBlock::Text {
                text: text.to_string(),
            },
        ],
    };
    append_line(
        &session.path.join("messages.jsonl"),
        &message_to_jsonl(&message)?,
    )?;
    touch_session(session)?;
    Ok(message)
}

pub async fn append_tool_result_message_async(
    session: Session,
    call_id: String,
    status: ToolResultStatus,
    text: String,
) -> Result<Message, StorageError> {
    run_blocking_storage(move || append_tool_result_message(&session, &call_id, status, &text))
        .await
}

pub fn append_event(session: &Session, event: Event) -> Result<(), StorageError> {
    let path = session.path.join("events.jsonl");
    let mut sequence = session
        .sequence
        .lock()
        .map_err(|_| StorageError::SequenceLock)?;
    append_line(&path, &event_to_jsonl(*sequence, &session.id, &event)?)?;
    *sequence += 1;
    touch_session(session)
}

pub async fn append_event_async(session: Session, event: Event) -> Result<(), StorageError> {
    run_blocking_storage(move || append_event(&session, event)).await
}

pub fn finalize_session(session: &Session, outcome: Outcome) -> Result<(), StorageError> {
    let path = session.path.join("session.json");
    let content = fs::read_to_string(&path)?;
    let mut record: SessionRecord = serde_json::from_str(&content)?;
    record.status = match outcome {
        Outcome::Succeeded => SessionStatus::Succeeded,
        Outcome::Failed => SessionStatus::Failed,
        Outcome::Cancelled => SessionStatus::Cancelled,
    };
    record.updated_at = timestamp();
    let session_json = serde_json::to_string(&record)?;
    fs::write(path, format!("{session_json}\n"))?;
    Ok(())
}

fn touch_session(session: &Session) -> Result<(), StorageError> {
    let path = session.path.join("session.json");
    let content = fs::read_to_string(&path)?;
    let mut record: SessionRecord = serde_json::from_str(&content)?;
    record.updated_at = timestamp();
    let session_json = serde_json::to_string(&record)?;
    fs::write(path, format!("{session_json}\n"))?;
    Ok(())
}

pub async fn finalize_session_async(
    session: Session,
    outcome: Outcome,
) -> Result<(), StorageError> {
    run_blocking_storage(move || finalize_session(&session, outcome)).await
}

pub fn replay_events(path: &Path) -> Result<Vec<String>, StorageError> {
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

fn append_line(path: &Path, line: &str) -> Result<(), StorageError> {
    let mut file = OpenOptions::new().append(true).create(true).open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

async fn run_blocking_storage<T, F>(operation: F) -> Result<T, StorageError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, StorageError> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| StorageError::TaskJoin(error.to_string()))?
}

fn message_to_jsonl(message: &Message) -> Result<String, StorageError> {
    Ok(serde_json::to_string(&MessageRecord {
        version: "1".to_string(),
        message: message.clone(),
    })?)
}

fn event_to_jsonl(sequence: u64, session_id: &str, event: &Event) -> Result<String, StorageError> {
    Ok(serde_json::to_string(&EventRecord {
        version: "1".to_string(),
        sequence,
        timestamp: timestamp(),
        session_id: session_id.to_string(),
        event: event.clone(),
    })?)
}

fn next_sequence(path: &Path) -> Result<u64, StorageError> {
    if !path.exists() {
        return Ok(1);
    }
    let content = fs::read_to_string(path)?;
    Ok(content.lines().count() as u64 + 1)
}

fn stable_workspace_id(root: &Path) -> u64 {
    root.display()
        .to_string()
        .bytes()
        .fold(14_695_981_039_346_656_037_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(1_099_511_628_211)
        })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WorkspaceRecord {
    version: String,
    workspace_id: String,
    root: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SessionRecord {
    version: String,
    session_id: String,
    workspace_root: String,
    created_at: String,
    updated_at: String,
    status: SessionStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct MessageRecord {
    version: String,
    message: Message,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct EventRecord {
    version: String,
    sequence: u64,
    timestamp: String,
    session_id: String,
    event: Event,
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", timestamp_nanos())
}

fn timestamp() -> String {
    timestamp_nanos().to_string()
}

fn timestamp_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_session_should_write_required_files() {
        let root = temp_dir("session_files");
        fs::create_dir_all(&root).unwrap();

        let session = create_session(&root).unwrap();

        assert!(session.path.join("session.json").exists());
    }

    #[test]
    fn append_user_message_should_write_message_and_event() {
        let root = temp_dir("append_user");
        fs::create_dir_all(&root).unwrap();
        let session = create_session(&root).unwrap();

        append_user_message(&session, "hello").unwrap();

        let messages = fs::read_to_string(session.path.join("messages.jsonl")).unwrap();
        assert!(messages.contains("\"role\":\"user\""));
    }

    #[test]
    fn replay_events_should_return_event_timeline() {
        let root = temp_dir("replay");
        fs::create_dir_all(&root).unwrap();
        let session = create_session(&root).unwrap();

        let timeline = replay_events(&session.path.join("events.jsonl")).unwrap();

        assert_eq!(timeline, vec!["1: session_started"]);
    }

    #[test]
    fn event_schema_should_match_golden() {
        let root = temp_dir("event_schema_golden");
        let session = test_session(&root, "session_golden");

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
            normalize_json_string_field(&actual, "timestamp", "<timestamp>"),
            include_str!("../tests/golden/events_v1.jsonl")
        );
    }

    #[test]
    fn message_schema_should_match_golden() {
        let root = temp_dir("message_schema_golden");
        let session = test_session(&root, "session_messages");

        append_user_message(&session, "hello \"world\"").unwrap();
        append_assistant_message(
            &session,
            "",
            "I will read",
            &[(
                "call_1".to_string(),
                "Read".to_string(),
                serde_json::json!({"path": "src/lib.rs"}),
            )],
        )
        .unwrap();
        append_tool_result_message(&session, "call_1", ToolResultStatus::Success, "done").unwrap();

        let actual = fs::read_to_string(session.path.join("messages.jsonl")).unwrap();
        let normalized = normalize_json_string_field(&actual, "id", "<message_id>");
        let normalized = normalize_json_string_field(&normalized, "created_at", "<timestamp>");

        assert_eq!(
            normalized,
            include_str!("../tests/golden/messages_v1.jsonl")
        );
    }

    #[test]
    fn streaming_events_should_not_commit_interrupted_assistant_message() {
        let root = temp_dir("interrupted_turn");
        fs::create_dir_all(&root).unwrap();
        let session = create_session(&root).unwrap();

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
    fn load_session_should_reject_workspace_mismatch() {
        let root = temp_dir("resume_a");
        let other = temp_dir("resume_b");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&other).unwrap();
        let session = create_session(&root).unwrap();
        let session_id = session.id.clone();
        let other_session_dir = other.join(".flash/sessions").join(&session_id);
        fs::create_dir_all(&other_session_dir).unwrap();
        fs::copy(
            session.path.join("session.json"),
            other_session_dir.join("session.json"),
        )
        .unwrap();

        let error = load_session(&other, &session_id).unwrap_err();

        assert!(matches!(error, StorageError::WorkspaceMismatch { .. }));
    }

    #[test]
    fn create_session_should_not_require_or_write_index_json() {
        let root = temp_dir("no_index");
        fs::create_dir_all(&root).unwrap();

        create_session(&root).unwrap();

        assert!(!root
            .join(".flash")
            .join("sessions")
            .join("index.json")
            .exists());
    }

    fn test_session(root: &Path, id: &str) -> Session {
        let path = root.join(".flash").join("sessions").join(id);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("events.jsonl"), "").unwrap();
        fs::write(path.join("messages.jsonl"), "").unwrap();
        fs::write(
            path.join("session.json"),
            serde_json::to_string(&SessionRecord {
                version: "1".to_string(),
                session_id: id.to_string(),
                workspace_root: root.display().to_string(),
                created_at: "0".to_string(),
                updated_at: "0".to_string(),
                status: SessionStatus::Running,
            })
            .unwrap(),
        )
        .unwrap();
        Session {
            id: id.to_string(),
            workspace_root: root.to_path_buf(),
            path,
            sequence: Arc::new(Mutex::new(1)),
            finalizing: Arc::new(AtomicBool::new(false)),
        }
    }

    fn normalize_json_string_field(content: &str, key: &str, replacement: &str) -> String {
        let needle = format!("\"{key}\":\"");
        let mut normalized = String::with_capacity(content.len());
        let mut rest = content;
        while let Some(start) = rest.find(&needle) {
            normalized.push_str(&rest[..start + needle.len()]);
            normalized.push_str(replacement);
            rest = &rest[start + needle.len()..];
            let Some(end) = rest.find('"') else {
                normalized.push_str(rest);
                return normalized;
            };
            rest = &rest[end..];
        }
        normalized.push_str(rest);
        normalized
    }

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("flash_core_{name}_{}", timestamp_nanos()))
    }
}
