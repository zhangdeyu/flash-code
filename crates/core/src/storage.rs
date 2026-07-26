use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageLimits {
    pub max_event_bytes: usize,
    pub max_jsonl_bytes: u64,
}

impl Default for StorageLimits {
    fn default() -> Self {
        Self {
            max_event_bytes: 1024 * 1024,
            max_jsonl_bytes: 50 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub workspace_root: PathBuf,
    pub path: PathBuf,
    pub parent_session_id: Option<String>,
    pub status: SessionStatus,
    pub owner_pid: Option<u32>,
    pub limits: StorageLimits,
    sequence: Arc<Mutex<u64>>,
    finalizing: Arc<AtomicBool>,
}

impl PartialEq for Session {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.workspace_root == other.workspace_root
            && self.path == other.path
            && self.parent_session_id == other.parent_session_id
            && self.status == other.status
            && self.owner_pid == other.owner_pid
            && self.limits == other.limits
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
    WorkspaceMismatch {
        expected: PathBuf,
        actual: PathBuf,
    },
    SessionStillRunning(String),
    AncestryCycle(String),
    CorruptJsonl {
        path: PathBuf,
        line: usize,
        message: String,
    },
    ResourceLimit {
        resource: String,
        limit: u64,
    },
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
            Self::SessionStillRunning(session_id) => {
                write!(formatter, "session `{session_id}` is still running")
            }
            Self::AncestryCycle(session_id) => {
                write!(
                    formatter,
                    "session ancestry contains a cycle at `{session_id}`"
                )
            }
            Self::CorruptJsonl {
                path,
                line,
                message,
            } => write!(
                formatter,
                "corrupt JSONL at {} line {line}: {message}",
                path.display()
            ),
            Self::ResourceLimit { resource, limit } => {
                write!(
                    formatter,
                    "{resource} exceeded configured limit of {limit} bytes"
                )
            }
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
    atomic_write(&flash_dir.join("workspace.json"), &workspace_json)?;
    Ok(workspace)
}

pub fn create_session(root: &Path) -> Result<Session, StorageError> {
    create_session_with_limits(root, StorageLimits::default())
}

pub fn create_session_with_limits(
    root: &Path,
    limits: StorageLimits,
) -> Result<Session, StorageError> {
    create_session_record(root, None, limits)
}

pub fn create_continuation_session(
    root: &Path,
    parent_session_id: &str,
) -> Result<Session, StorageError> {
    create_continuation_session_with_limits(root, parent_session_id, StorageLimits::default())
}

pub fn create_continuation_session_with_limits(
    root: &Path,
    parent_session_id: &str,
    limits: StorageLimits,
) -> Result<Session, StorageError> {
    let parent = load_session(root, parent_session_id)?;
    if parent.status == SessionStatus::Running {
        return Err(StorageError::SessionStillRunning(
            parent_session_id.to_string(),
        ));
    }
    create_session_record(root, Some(parent_session_id.to_string()), limits)
}

fn create_session_record(
    root: &Path,
    parent_session_id: Option<String>,
    limits: StorageLimits,
) -> Result<Session, StorageError> {
    init_workspace(root)?;
    let id = new_id("session");
    let session_dir = root.join(".flash").join("sessions").join(&id);
    fs::create_dir_all(session_dir.join("artifacts"))?;
    let session = Session {
        id: id.clone(),
        workspace_root: root.to_path_buf(),
        path: session_dir.clone(),
        parent_session_id: parent_session_id.clone(),
        status: SessionStatus::Running,
        owner_pid: Some(std::process::id()),
        limits,
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
        parent_session_id,
        owner_pid: Some(std::process::id()),
    })?;
    atomic_write(&session_dir.join("session.json"), &session_json)?;
    File::create(session_dir.join("messages.jsonl"))?.sync_all()?;
    File::create(session_dir.join("events.jsonl"))?.sync_all()?;
    append_event(&session, Event::SessionStarted { session_id: id })?;
    Ok(session)
}

pub async fn create_session_async(root: PathBuf) -> Result<Session, StorageError> {
    run_blocking_storage(move || create_session(&root)).await
}

pub async fn create_session_with_limits_async(
    root: PathBuf,
    limits: StorageLimits,
) -> Result<Session, StorageError> {
    run_blocking_storage(move || create_session_with_limits(&root, limits)).await
}

pub async fn create_continuation_session_async(
    root: PathBuf,
    parent_session_id: String,
) -> Result<Session, StorageError> {
    run_blocking_storage(move || create_continuation_session(&root, &parent_session_id)).await
}

pub async fn create_continuation_session_with_limits_async(
    root: PathBuf,
    parent_session_id: String,
    limits: StorageLimits,
) -> Result<Session, StorageError> {
    run_blocking_storage(move || {
        create_continuation_session_with_limits(&root, &parent_session_id, limits)
    })
    .await
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
        session.limits.max_jsonl_bytes,
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
    let limits = StorageLimits::default();
    repair_jsonl::<MessageRecord>(&session_dir.join("messages.jsonl"), limits.max_jsonl_bytes)?;
    repair_jsonl::<EventRecord>(&session_dir.join("events.jsonl"), limits.max_jsonl_bytes)?;
    let sequence = next_sequence(&session_dir.join("events.jsonl"))?;
    let finalized = record.status != SessionStatus::Running;
    Ok(Session {
        id: session_id.to_string(),
        workspace_root: root.to_path_buf(),
        path: session_dir,
        parent_session_id: record.parent_session_id,
        status: record.status,
        owner_pid: record.owner_pid,
        limits,
        sequence: Arc::new(Mutex::new(sequence)),
        finalizing: Arc::new(AtomicBool::new(finalized)),
    })
}

pub fn load_session_messages(session: &Session) -> Result<Vec<Message>, StorageError> {
    let path = session.path.join("messages.jsonl");
    repair_jsonl::<MessageRecord>(&path, session.limits.max_jsonl_bytes)?;
    let content = fs::read_to_string(path)?;
    content
        .lines()
        .map(|line| {
            let record: MessageRecord = serde_json::from_str(line)?;
            Ok(record.message)
        })
        .collect()
}

pub fn load_session_history(root: &Path, session_id: &str) -> Result<Vec<Message>, StorageError> {
    let mut ancestry = Vec::new();
    let mut visited = BTreeSet::new();
    let mut current = Some(session_id.to_string());
    while let Some(current_id) = current {
        if !visited.insert(current_id.clone()) {
            return Err(StorageError::AncestryCycle(current_id));
        }
        let session = load_session(root, &current_id)?;
        current = session.parent_session_id.clone();
        ancestry.push(session);
    }
    ancestry.reverse();

    let mut history = Vec::new();
    for session in ancestry {
        history.extend(load_session_messages(&session)?);
    }
    Ok(history)
}

pub async fn load_session_history_async(
    root: PathBuf,
    session_id: String,
) -> Result<Vec<Message>, StorageError> {
    run_blocking_storage(move || load_session_history(&root, &session_id)).await
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
        session.limits.max_jsonl_bytes,
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
        session.limits.max_jsonl_bytes,
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
        session.limits.max_jsonl_bytes,
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
    record.owner_pid = None;
    let session_json = serde_json::to_string(&record)?;
    sync_file(&session.path.join("messages.jsonl"))?;
    sync_file(&session.path.join("events.jsonl"))?;
    atomic_write(&path, &session_json)?;
    Ok(())
}

fn touch_session(session: &Session) -> Result<(), StorageError> {
    let path = session.path.join("session.json");
    let content = fs::read_to_string(&path)?;
    let mut record: SessionRecord = serde_json::from_str(&content)?;
    record.updated_at = timestamp();
    let session_json = serde_json::to_string(&record)?;
    atomic_write(&path, &session_json)?;
    Ok(())
}

pub async fn finalize_session_async(
    session: Session,
    outcome: Outcome,
) -> Result<(), StorageError> {
    run_blocking_storage(move || finalize_session(&session, outcome)).await
}

pub fn replay_events(path: &Path) -> Result<Vec<String>, StorageError> {
    repair_jsonl::<EventRecord>(path, StorageLimits::default().max_jsonl_bytes)?;
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

pub fn recover_session(root: &Path, session_id: &str) -> Result<Session, StorageError> {
    let session = load_session(root, session_id)?;
    if session.status != SessionStatus::Running {
        return Ok(session);
    }

    let records = read_event_records(
        &session.path.join("events.jsonl"),
        session.limits.max_jsonl_bytes,
    )?;
    let finished = records
        .iter()
        .filter_map(|record| match record.event {
            Event::SessionFinished { outcome } => Some(outcome),
            _ => None,
        })
        .collect::<Vec<_>>();
    if finished.len() > 1 {
        return Err(StorageError::Parse(format!(
            "session `{session_id}` contains multiple session_finished events"
        )));
    }
    if let Some(outcome) = finished.first() {
        finalize_session(&session, *outcome)?;
        return load_session(root, session_id);
    }
    if session.owner_pid.is_some_and(process_alive) {
        return Ok(session);
    }

    append_event(
        &session,
        Event::Error {
            message: "session recovered after previous process exited unexpectedly".to_string(),
        },
    )?;
    append_event(
        &session,
        Event::SessionFinished {
            outcome: Outcome::Failed,
        },
    )?;
    finalize_session(&session, Outcome::Failed)?;
    load_session(root, session_id)
}

pub async fn recover_session_async(
    root: PathBuf,
    session_id: String,
) -> Result<Session, StorageError> {
    run_blocking_storage(move || recover_session(&root, &session_id)).await
}

pub fn recover_workspace_sessions(root: &Path) -> Result<Vec<Session>, StorageError> {
    let sessions_dir = root.join(".flash").join("sessions");
    if !sessions_dir.exists() {
        return Ok(Vec::new());
    }
    let mut session_ids = Vec::new();
    for entry in fs::read_dir(sessions_dir)? {
        let entry = entry?;
        if entry.path().join("session.json").is_file() {
            if let Some(session_id) = entry.file_name().to_str() {
                session_ids.push(session_id.to_string());
            }
        }
    }
    session_ids.sort();
    let mut recovered = Vec::new();
    for session_id in session_ids {
        match recover_session(root, &session_id) {
            Ok(session) => recovered.push(session),
            Err(StorageError::WorkspaceMismatch { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(recovered)
}

fn append_line(path: &Path, line: &str, max_bytes: u64) -> Result<(), StorageError> {
    let mut file = OpenOptions::new().append(true).create(true).open(path)?;
    let additional = line.len().saturating_add(1) as u64;
    let current = file.metadata()?.len();
    if current.saturating_add(additional) > max_bytes {
        return Err(StorageError::ResourceLimit {
            resource: path.display().to_string(),
            limit: max_bytes,
        });
    }
    writeln!(file, "{line}")?;
    file.flush()?;
    Ok(())
}

fn read_event_records(path: &Path, max_bytes: u64) -> Result<Vec<EventRecord>, StorageError> {
    repair_jsonl::<EventRecord>(path, max_bytes)?;
    fs::read_to_string(path)?
        .lines()
        .map(|line| serde_json::from_str(line).map_err(StorageError::from))
        .collect()
}

fn repair_jsonl<T>(path: &Path, max_bytes: u64) -> Result<bool, StorageError>
where
    T: DeserializeOwned,
{
    if !path.exists() {
        return Ok(false);
    }
    if path.metadata()?.len() > max_bytes {
        return Err(StorageError::ResourceLimit {
            resource: path.display().to_string(),
            limit: max_bytes,
        });
    }
    let bytes = fs::read(path)?;
    if bytes.is_empty() {
        return Ok(false);
    }

    let mut offset = 0;
    let mut line = 1;
    while offset < bytes.len() {
        let relative_end = bytes[offset..].iter().position(|byte| *byte == b'\n');
        let (end, next_offset) = match relative_end {
            Some(relative_end) => {
                let end = offset + relative_end;
                (end, end + 1)
            }
            None => (bytes.len(), bytes.len()),
        };
        if let Err(error) = serde_json::from_slice::<T>(&bytes[offset..end]) {
            if next_offset == bytes.len() {
                let file = OpenOptions::new().write(true).open(path)?;
                file.set_len(offset as u64)?;
                file.sync_all()?;
                return Ok(true);
            }
            return Err(StorageError::CorruptJsonl {
                path: path.to_path_buf(),
                line,
                message: error.to_string(),
            });
        }
        offset = next_offset;
        line += 1;
    }

    if !bytes.ends_with(b"\n") {
        let mut file = OpenOptions::new().append(true).open(path)?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_all()?;
        return Ok(true);
    }
    Ok(false)
}

fn atomic_write(path: &Path, content: &str) -> Result<(), StorageError> {
    atomic_write_with(path, content, || Ok(()))
}

fn atomic_write_with<F>(path: &Path, content: &str, before_rename: F) -> Result<(), StorageError>
where
    F: FnOnce() -> io::Result<()>,
{
    let parent = path.parent().ok_or_else(|| {
        StorageError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic write target has no parent directory",
        ))
    })?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let temp_path = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        timestamp_nanos()
    ));
    let result = (|| -> Result<(), StorageError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(content.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_all()?;
        before_rename()?;
        fs::rename(&temp_path, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _result = fs::remove_file(&temp_path);
    }
    result
}

fn sync_file(path: &Path) -> Result<(), StorageError> {
    OpenOptions::new().read(true).open(path)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn process_alive(pid: u32) -> bool {
    pid == std::process::id()
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner_pid: Option<u32>,
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

    #[test]
    fn continuation_should_create_child_without_modifying_parent() {
        let root = temp_dir("continuation_child");
        fs::create_dir_all(&root).unwrap();
        let parent = create_session(&root).unwrap();
        append_system_message(&parent, "system").unwrap();
        append_user_message(&parent, "parent task").unwrap();
        append_assistant_message(&parent, "", "parent answer", &[]).unwrap();
        append_event(
            &parent,
            Event::SessionFinished {
                outcome: Outcome::Succeeded,
            },
        )
        .unwrap();
        finalize_session(&parent, Outcome::Succeeded).unwrap();
        let parent_metadata = fs::read(parent.path.join("session.json")).unwrap();
        let parent_messages = fs::read(parent.path.join("messages.jsonl")).unwrap();
        let parent_events = fs::read(parent.path.join("events.jsonl")).unwrap();

        let child = create_continuation_session(&root, &parent.id).unwrap();

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
        let child_metadata = fs::read_to_string(child.path.join("session.json")).unwrap();
        assert!(child_metadata.contains(&format!("\"parent_session_id\":\"{}\"", parent.id)));
    }

    #[test]
    fn continuation_should_reject_running_parent() {
        let root = temp_dir("continuation_running");
        fs::create_dir_all(&root).unwrap();
        let parent = create_session(&root).unwrap();

        let error = create_continuation_session(&root, &parent.id).unwrap_err();

        assert!(matches!(error, StorageError::SessionStillRunning(id) if id == parent.id));
    }

    #[test]
    fn history_should_follow_multiple_continuation_generations() {
        let root = temp_dir("continuation_history");
        fs::create_dir_all(&root).unwrap();
        let parent = create_session(&root).unwrap();
        append_system_message(&parent, "system").unwrap();
        append_user_message(&parent, "parent task").unwrap();
        append_assistant_message(&parent, "", "parent answer", &[]).unwrap();
        finalize_session(&parent, Outcome::Succeeded).unwrap();

        let child = create_continuation_session(&root, &parent.id).unwrap();
        append_user_message(&child, "child task").unwrap();
        append_assistant_message(&child, "", "child answer", &[]).unwrap();
        finalize_session(&child, Outcome::Succeeded).unwrap();

        let grandchild = create_continuation_session(&root, &child.id).unwrap();
        append_user_message(&grandchild, "grandchild task").unwrap();

        let history = load_session_history(&root, &grandchild.id).unwrap();
        let texts = history
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
                "system",
                "parent task",
                "parent answer",
                "child task",
                "child answer",
                "grandchild task"
            ]
        );
    }

    #[test]
    fn history_should_reject_ancestry_cycle() {
        let root = temp_dir("continuation_cycle");
        fs::create_dir_all(&root).unwrap();
        let session = create_session(&root).unwrap();
        finalize_session(&session, Outcome::Succeeded).unwrap();
        let path = session.path.join("session.json");
        let mut record: SessionRecord =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        record.parent_session_id = Some(session.id.clone());
        fs::write(&path, serde_json::to_string(&record).unwrap()).unwrap();

        let error = load_session_history(&root, &session.id).unwrap_err();

        assert!(matches!(error, StorageError::AncestryCycle(id) if id == session.id));
    }

    #[test]
    fn atomic_write_failure_should_preserve_previous_metadata() {
        let root = temp_dir("atomic_metadata_failure");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("session.json");
        atomic_write(&path, r#"{"status":"running"}"#).unwrap();

        let error = atomic_write_with(&path, r#"{"status":"failed"}"#, || {
            Err(io::Error::other("injected before rename"))
        })
        .unwrap_err();

        assert!(matches!(error, StorageError::Io(_)));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"status\":\"running\"}\n"
        );
        let names = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["session.json"]);
    }

    #[test]
    fn load_should_repair_only_truncated_jsonl_tails() {
        let root = temp_dir("repair_truncated_tails");
        fs::create_dir_all(&root).unwrap();
        let session = create_session(&root).unwrap();
        append_user_message(&session, "preserved").unwrap();
        let messages_path = session.path.join("messages.jsonl");
        let events_path = session.path.join("events.jsonl");
        let messages_prefix = fs::read(&messages_path).unwrap();
        let events_prefix = fs::read(&events_path).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&messages_path)
            .unwrap()
            .write_all(br#"{"version":"1","message":"#)
            .unwrap();
        OpenOptions::new()
            .append(true)
            .open(&events_path)
            .unwrap()
            .write_all(br#"{"version":"1","sequence":"#)
            .unwrap();

        let loaded = load_session(&root, &session.id).unwrap();
        let messages = load_session_messages(&loaded).unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(fs::read(messages_path).unwrap(), messages_prefix);
        assert_eq!(fs::read(events_path).unwrap(), events_prefix);
        assert_eq!(
            replay_events(&loaded.path.join("events.jsonl")).unwrap(),
            vec!["1: session_started", "2: user_message_appended"]
        );
    }

    #[test]
    fn load_should_reject_jsonl_corruption_before_last_record() {
        let root = temp_dir("reject_middle_corruption");
        fs::create_dir_all(&root).unwrap();
        let session = create_session(&root).unwrap();
        let events_path = session.path.join("events.jsonl");
        let first = fs::read_to_string(&events_path).unwrap();
        let last = event_to_jsonl(
            3,
            &session.id,
            &Event::Error {
                message: "after corruption".to_string(),
            },
        )
        .unwrap();
        let corrupt = format!("{first}{{not-json}}\n{last}\n");
        fs::write(&events_path, &corrupt).unwrap();

        let error = load_session(&root, &session.id).unwrap_err();

        assert!(
            matches!(error, StorageError::CorruptJsonl { line: 2, .. }),
            "{error}"
        );
        assert_eq!(fs::read_to_string(events_path).unwrap(), corrupt);
    }

    #[test]
    fn recovery_should_fail_stale_session_once_and_allow_continuation() {
        let root = temp_dir("recover_stale");
        fs::create_dir_all(&root).unwrap();
        let session = create_session(&root).unwrap();
        append_user_message(&session, "unfinished task").unwrap();
        set_owner_pid(&session, Some(exited_pid()));
        OpenOptions::new()
            .append(true)
            .open(session.path.join("events.jsonl"))
            .unwrap()
            .write_all(br#"{"truncated":"#)
            .unwrap();

        let recovered = recover_session(&root, &session.id).unwrap();
        let recovered_again = recover_session(&root, &session.id).unwrap();
        let records = read_event_records(
            &session.path.join("events.jsonl"),
            session.limits.max_jsonl_bytes,
        )
        .unwrap();
        let recovery_errors = records
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::Error { message }
                        if message.contains("previous process exited unexpectedly")
                )
            })
            .count();
        let finished = records
            .iter()
            .filter(|record| matches!(record.event, Event::SessionFinished { .. }))
            .count();

        assert_eq!(recovered.status, SessionStatus::Failed);
        assert_eq!(recovered.owner_pid, None);
        assert_eq!(recovered_again.status, SessionStatus::Failed);
        assert_eq!(recovery_errors, 1);
        assert_eq!(finished, 1);
        assert!(!replay_events(&session.path.join("events.jsonl"))
            .unwrap()
            .is_empty());
        let child = create_continuation_session(&root, &session.id).unwrap();
        assert_eq!(
            child.parent_session_id.as_deref(),
            Some(session.id.as_str())
        );
    }

    #[test]
    fn recovery_should_not_fail_session_owned_by_live_process() {
        let root = temp_dir("recover_live");
        fs::create_dir_all(&root).unwrap();
        let session = create_session(&root).unwrap();

        let recovered = recover_session(&root, &session.id).unwrap();
        let error = create_continuation_session(&root, &session.id).unwrap_err();

        assert_eq!(recovered.status, SessionStatus::Running);
        assert_eq!(recovered.owner_pid, Some(std::process::id()));
        assert!(matches!(error, StorageError::SessionStillRunning(id) if id == session.id));
    }

    #[test]
    fn recovery_should_commit_existing_terminal_event_without_duplicate() {
        let root = temp_dir("recover_terminal_event");
        fs::create_dir_all(&root).unwrap();
        let session = create_session(&root).unwrap();
        append_event(
            &session,
            Event::SessionFinished {
                outcome: Outcome::Succeeded,
            },
        )
        .unwrap();
        set_owner_pid(&session, Some(exited_pid()));

        let recovered = recover_session(&root, &session.id).unwrap();
        let records = read_event_records(
            &session.path.join("events.jsonl"),
            session.limits.max_jsonl_bytes,
        )
        .unwrap();

        assert_eq!(recovered.status, SessionStatus::Succeeded);
        assert_eq!(recovered.owner_pid, None);
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.event, Event::SessionFinished { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn event_and_jsonl_limits_should_return_structured_errors() {
        let root = temp_dir("storage_limits");
        fs::create_dir_all(&root).unwrap();
        let mut session = create_session(&root).unwrap();
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
        let root = temp_dir("terminal_reserve");
        fs::create_dir_all(&root).unwrap();
        let mut session = create_session(&root).unwrap();
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
        finalize_session(&session, Outcome::Failed).unwrap();

        assert!(matches!(normal, StorageError::ResourceLimit { .. }));
        let loaded = load_session(&root, &session.id).unwrap();
        assert_eq!(loaded.status, SessionStatus::Failed);
        assert_eq!(
            fs::read_to_string(session.path.join("events.jsonl"))
                .unwrap()
                .matches("\"type\":\"session_finished\"")
                .count(),
            1
        );
    }

    #[test]
    fn message_jsonl_limit_should_stop_before_partial_write() {
        let root = temp_dir("message_jsonl_limit");
        fs::create_dir_all(&root).unwrap();
        let mut session = create_session(&root).unwrap();
        session.limits.max_jsonl_bytes = 32;

        let error = append_user_message(&session, &"x".repeat(128)).unwrap_err();

        assert!(matches!(error, StorageError::ResourceLimit { .. }));
        assert!(fs::read_to_string(session.path.join("messages.jsonl"))
            .unwrap()
            .is_empty());
    }

    fn set_owner_pid(session: &Session, owner_pid: Option<u32>) {
        let path = session.path.join("session.json");
        let mut record: SessionRecord =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        record.owner_pid = owner_pid;
        atomic_write(&path, &serde_json::to_string(&record).unwrap()).unwrap();
    }

    fn exited_pid() -> u32 {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
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
                parent_session_id: None,
                owner_pid: Some(std::process::id()),
            })
            .unwrap(),
        )
        .unwrap();
        Session {
            id: id.to_string(),
            workspace_root: root.to_path_buf(),
            path,
            parent_session_id: None,
            status: SessionStatus::Running,
            owner_pid: Some(std::process::id()),
            limits: StorageLimits::default(),
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
