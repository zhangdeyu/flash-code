use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::protocol::{Event, Message, Outcome, SessionStatus};

use super::atomic::{atomic_write, sync_file};
use super::event_log::{append_event, next_sequence};
use super::message_log::load_session_messages;
use super::recovery::repair_jsonl;
use super::{
    new_id, run_blocking_storage, stable_workspace_id, timestamp, EventRecord, MessageRecord,
    SessionRecord, StorageError, StorageLimits, WorkspaceRecord,
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
    pub parent_session_id: Option<String>,
    pub status: SessionStatus,
    pub owner_pid: Option<u32>,
    pub limits: StorageLimits,
    pub(super) sequence: Arc<Mutex<u64>>,
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
    fs::File::create(session_dir.join("messages.jsonl"))?.sync_all()?;
    fs::File::create(session_dir.join("events.jsonl"))?.sync_all()?;
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

/// Bump the session metadata `updated_at` timestamp atomically.
pub(super) fn touch_session(session: &Session) -> Result<(), StorageError> {
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

/// Construct a minimal in-memory session rooted at `root` with the given id.
/// Used by storage tests that need a session without running the full create path.
#[cfg(test)]
pub(in crate::storage) fn test_session(root: &Path, id: &str) -> Session {
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::protocol::{ContentBlock, Event, Outcome};
    use crate::storage::test_support::temp_dir;

    #[test]
    fn create_session_should_write_required_files() {
        let root = temp_dir("session_files");
        fs::create_dir_all(&root).unwrap();

        let session = create_session(&root).unwrap();

        assert!(session.path.join("session.json").exists());
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
        super::super::message_log::append_system_message(&parent, "system").unwrap();
        super::super::message_log::append_user_message(&parent, "parent task").unwrap();
        super::super::message_log::append_assistant_message(&parent, "", "parent answer", &[])
            .unwrap();
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
        super::super::message_log::append_system_message(&parent, "system").unwrap();
        super::super::message_log::append_user_message(&parent, "parent task").unwrap();
        super::super::message_log::append_assistant_message(&parent, "", "parent answer", &[])
            .unwrap();
        finalize_session(&parent, Outcome::Succeeded).unwrap();

        let child = create_continuation_session(&root, &parent.id).unwrap();
        super::super::message_log::append_user_message(&child, "child task").unwrap();
        super::super::message_log::append_assistant_message(&child, "", "child answer", &[])
            .unwrap();
        finalize_session(&child, Outcome::Succeeded).unwrap();

        let grandchild = create_continuation_session(&root, &child.id).unwrap();
        super::super::message_log::append_user_message(&grandchild, "grandchild task").unwrap();

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
        atomic_write(&path, &serde_json::to_string(&record).unwrap()).unwrap();

        let error = load_session_history(&root, &session.id).unwrap_err();

        assert!(matches!(error, StorageError::AncestryCycle(id) if id == session.id));
    }
}
