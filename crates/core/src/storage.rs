use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::protocol::{escape_json, ContentBlock, Event, Message, Role, SessionStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub id: String,
    pub root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub workspace_root: PathBuf,
    pub path: PathBuf,
}

#[derive(Debug)]
pub enum StorageError {
    Io(std::io::Error),
    Parse(String),
    WorkspaceMismatch { expected: PathBuf, actual: PathBuf },
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "storage io error: {error}"),
            Self::Parse(message) => write!(formatter, "storage parse error: {message}"),
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

pub fn init_workspace(root: &Path) -> Result<Workspace, StorageError> {
    let flash_dir = root.join(".flash");
    let sessions_dir = flash_dir.join("sessions");
    fs::create_dir_all(&sessions_dir)?;
    let workspace = Workspace {
        id: format!("workspace_{}", stable_workspace_id(root)),
        root: root.to_path_buf(),
    };
    let workspace_json = format!(
        "{{\"version\":\"1\",\"workspace_id\":\"{}\",\"root\":\"{}\"}}\n",
        escape_json(&workspace.id),
        escape_json(&root.display().to_string())
    );
    fs::write(flash_dir.join("workspace.json"), workspace_json)?;
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
    };
    let now = timestamp();
    let session_json = format!(
        concat!(
            "{{",
            "\"version\":\"1\",",
            "\"session_id\":\"{}\",",
            "\"workspace_root\":\"{}\",",
            "\"created_at\":\"{}\",",
            "\"updated_at\":\"{}\",",
            "\"status\":\"{}\"",
            "}}\n"
        ),
        escape_json(&id),
        escape_json(&root.display().to_string()),
        now,
        now,
        SessionStatus::Running.as_str()
    );
    fs::write(session_dir.join("session.json"), session_json)?;
    fs::write(session_dir.join("messages.jsonl"), "")?;
    fs::write(session_dir.join("events.jsonl"), "")?;
    append_event(&session, Event::SessionStarted { session_id: id })?;
    Ok(session)
}

pub fn load_session(root: &Path, session_id: &str) -> Result<Session, StorageError> {
    let session_dir = root.join(".flash").join("sessions").join(session_id);
    let content = fs::read_to_string(session_dir.join("session.json"))?;
    let workspace_root = find_json_string(&content, "workspace_root")
        .ok_or_else(|| StorageError::Parse("missing workspace_root".to_string()))?;
    let expected = PathBuf::from(workspace_root);
    if expected != root {
        return Err(StorageError::WorkspaceMismatch {
            expected,
            actual: root.to_path_buf(),
        });
    }
    Ok(Session {
        id: session_id.to_string(),
        workspace_root: root.to_path_buf(),
        path: session_dir,
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
        &message_to_jsonl(&message),
    )?;
    append_event(
        session,
        Event::UserMessageAppended {
            message_id: message.id.clone(),
        },
    )?;
    Ok(message)
}

pub fn append_event(session: &Session, event: Event) -> Result<(), StorageError> {
    let path = session.path.join("events.jsonl");
    let sequence = next_sequence(&path)?;
    append_line(&path, &event_to_jsonl(sequence, &session.id, &event))
}

pub fn replay_events(path: &Path) -> Result<Vec<String>, StorageError> {
    let content = fs::read_to_string(path)?;
    Ok(content
        .lines()
        .filter_map(|line| {
            let sequence = find_json_number(line, "sequence")?;
            let event_type = find_json_string(line, "type")?;
            Some(format!("{sequence}: {event_type}"))
        })
        .collect())
}

fn append_line(path: &Path, line: &str) -> Result<(), StorageError> {
    let mut file = OpenOptions::new().append(true).create(true).open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

fn message_to_jsonl(message: &Message) -> String {
    let content = match message.content.first() {
        Some(ContentBlock::Text { text }) => {
            format!("{{\"type\":\"text\",\"text\":\"{}\"}}", escape_json(text))
        }
        _ => "{\"type\":\"text\",\"text\":\"\"}".to_string(),
    };
    format!(
        concat!(
            "{{\"version\":\"1\",\"message\":{{",
            "\"id\":\"{}\",",
            "\"role\":\"{}\",",
            "\"created_at\":\"{}\",",
            "\"content\":[{}]",
            "}}}}"
        ),
        escape_json(&message.id),
        message.role.as_str(),
        escape_json(&message.created_at),
        content
    )
}

fn event_to_jsonl(sequence: u64, session_id: &str, event: &Event) -> String {
    let body = match event {
        Event::SessionStarted { session_id } => {
            format!(
                "{{\"type\":\"session_started\",\"session_id\":\"{}\"}}",
                escape_json(session_id)
            )
        }
        Event::UserMessageAppended { message_id } => {
            format!(
                "{{\"type\":\"user_message_appended\",\"message_id\":\"{}\"}}",
                escape_json(message_id)
            )
        }
        Event::Error { message } => {
            format!(
                "{{\"type\":\"error\",\"message\":\"{}\"}}",
                escape_json(message)
            )
        }
        other => format!("{{\"type\":\"{}\"}}", other.event_type()),
    };
    format!(
        concat!(
            "{{\"version\":\"1\",",
            "\"sequence\":{},",
            "\"timestamp\":\"{}\",",
            "\"session_id\":\"{}\",",
            "\"event\":{}",
            "}}"
        ),
        sequence,
        timestamp(),
        escape_json(session_id),
        body
    )
}

fn next_sequence(path: &Path) -> Result<u64, StorageError> {
    if !path.exists() {
        return Ok(1);
    }
    let content = fs::read_to_string(path)?;
    Ok(content.lines().count() as u64 + 1)
}

fn find_json_string(content: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = content.find(&needle)? + needle.len();
    let rest = &content[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn find_json_number(content: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":");
    let start = content.find(&needle)? + needle.len();
    let rest = &content[start..];
    let end = rest
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

fn stable_workspace_id(root: &Path) -> u64 {
    root.display()
        .to_string()
        .bytes()
        .fold(14_695_981_039_346_656_037_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(1_099_511_628_211)
        })
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

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("flash_core_{name}_{}", timestamp_nanos()))
    }
}
