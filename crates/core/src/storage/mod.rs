//! Persistent session storage: JSONL message/event logs, atomic metadata writes,
//! crash recovery and resource limits.
//!
//! The public surface is re-exported from this module so downstream crates can
//! keep using `flash_core::storage::<Item>` unchanged.

mod atomic;
mod event_log;
mod limits;
mod message_log;
mod recovery;
mod session;

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::protocol::{Event, Message, SessionStatus};

pub use event_log::{append_event, append_event_async, replay_events};
pub use limits::StorageLimits;
pub use message_log::{
    append_assistant_message, append_assistant_message_async, append_system_message,
    append_system_message_async, append_tool_result_message, append_tool_result_message_async,
    append_user_message, append_user_message_async, load_session_messages,
};
pub use recovery::{recover_session, recover_session_async, recover_workspace_sessions};
pub use session::{
    create_continuation_session, create_continuation_session_async,
    create_continuation_session_with_limits, create_continuation_session_with_limits_async,
    create_session, create_session_async, create_session_with_limits,
    create_session_with_limits_async, finalize_session, finalize_session_async, init_workspace,
    load_session, load_session_history, load_session_history_async, Session, Workspace,
};

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

// --- Shared internal records ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::storage) struct WorkspaceRecord {
    version: String,
    workspace_id: String,
    root: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::storage) struct SessionRecord {
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
pub(in crate::storage) struct MessageRecord {
    version: String,
    message: Message,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::storage) struct EventRecord {
    version: String,
    sequence: u64,
    timestamp: String,
    session_id: String,
    event: Event,
}

// --- Shared internal helpers ---

/// Append a single JSONL line, enforcing a total file size limit.
pub(in crate::storage) fn append_line(
    path: &Path,
    line: &str,
    max_bytes: u64,
) -> Result<(), StorageError> {
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

/// Run a blocking storage operation on the tokio blocking thread pool.
pub(in crate::storage) async fn run_blocking_storage<T, F>(operation: F) -> Result<T, StorageError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, StorageError> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| StorageError::TaskJoin(error.to_string()))?
}

pub(in crate::storage) fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", timestamp_nanos())
}

pub(in crate::storage) fn timestamp() -> String {
    timestamp_nanos().to_string()
}

pub(in crate::storage) fn timestamp_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

pub(in crate::storage) fn stable_workspace_id(root: &Path) -> u64 {
    root.display()
        .to_string()
        .bytes()
        .fold(14_695_981_039_346_656_037_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(1_099_511_628_211)
        })
}

#[cfg(test)]
mod test_support {
    use std::path::PathBuf;

    use super::timestamp_nanos;

    pub(super) fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("flash_core_{name}_{}", timestamp_nanos()))
    }

    pub(super) fn normalize_json_string_field(
        content: &str,
        key: &str,
        replacement: &str,
    ) -> String {
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
}
