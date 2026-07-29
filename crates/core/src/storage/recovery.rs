use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

use crate::protocol::{Event, Outcome, SessionStatus};

use super::event_log::append_event;
use super::session::Session;
use super::session::{finalize_session, load_session};
use super::{run_blocking_storage, EventRecord, StorageError};

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

/// Repair a JSONL file by truncating an incomplete trailing record. Returns
/// `true` if the file was modified. A corrupt record that is not the last line
/// is a hard error.
pub(super) fn repair_jsonl<T>(path: &Path, max_bytes: u64) -> Result<bool, StorageError>
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

fn read_event_records(path: &Path, max_bytes: u64) -> Result<Vec<EventRecord>, StorageError> {
    repair_jsonl::<EventRecord>(path, max_bytes)?;
    fs::read_to_string(path)?
        .lines()
        .map(|line| serde_json::from_str(line).map_err(StorageError::from))
        .collect()
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

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};

    use super::*;
    use crate::protocol::{Event, Outcome, SessionStatus};
    use crate::storage::replay_events;

    #[test]
    fn load_should_repair_only_truncated_jsonl_tails() {
        let root = super::super::test_support::temp_dir("repair_truncated_tails");
        fs::create_dir_all(&root).unwrap();
        let session = super::super::session::create_session(&root).unwrap();
        super::super::message_log::append_user_message(&session, "preserved").unwrap();
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

        let loaded = super::super::session::load_session(&root, &session.id).unwrap();
        let messages = super::super::message_log::load_session_messages(&loaded).unwrap();

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
        let root = super::super::test_support::temp_dir("reject_middle_corruption");
        fs::create_dir_all(&root).unwrap();
        let session = super::super::session::create_session(&root).unwrap();
        let events_path = session.path.join("events.jsonl");
        let first = fs::read_to_string(&events_path).unwrap();
        let last = crate::storage::event_log::event_to_jsonl(
            3,
            &session.id,
            &Event::Error {
                message: "after corruption".to_string(),
            },
        )
        .unwrap();
        let corrupt = format!("{first}{{not-json}}\n{last}\n");
        fs::write(&events_path, &corrupt).unwrap();

        let error = super::super::session::load_session(&root, &session.id).unwrap_err();

        assert!(
            matches!(error, StorageError::CorruptJsonl { line: 2, .. }),
            "{error}"
        );
        assert_eq!(fs::read_to_string(events_path).unwrap(), corrupt);
    }

    #[test]
    fn recovery_should_fail_stale_session_once_and_allow_continuation() {
        let root = super::super::test_support::temp_dir("recover_stale");
        fs::create_dir_all(&root).unwrap();
        let session = super::super::session::create_session(&root).unwrap();
        super::super::message_log::append_user_message(&session, "unfinished task").unwrap();
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
        let child = super::super::session::create_continuation_session(&root, &session.id).unwrap();
        assert_eq!(
            child.parent_session_id.as_deref(),
            Some(session.id.as_str())
        );
    }

    #[test]
    fn recovery_should_not_fail_session_owned_by_live_process() {
        let root = super::super::test_support::temp_dir("recover_live");
        fs::create_dir_all(&root).unwrap();
        let session = super::super::session::create_session(&root).unwrap();

        let recovered = recover_session(&root, &session.id).unwrap();
        let error =
            super::super::session::create_continuation_session(&root, &session.id).unwrap_err();

        assert_eq!(recovered.status, SessionStatus::Running);
        assert_eq!(recovered.owner_pid, Some(std::process::id()));
        assert!(matches!(error, StorageError::SessionStillRunning(id) if id == session.id));
    }

    #[test]
    fn recovery_should_commit_existing_terminal_event_without_duplicate() {
        let root = super::super::test_support::temp_dir("recover_terminal_event");
        fs::create_dir_all(&root).unwrap();
        let session = super::super::session::create_session(&root).unwrap();
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

    fn set_owner_pid(session: &Session, owner_pid: Option<u32>) {
        let path = session.path.join("session.json");
        let mut record: crate::storage::SessionRecord =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        record.owner_pid = owner_pid;
        super::super::atomic::atomic_write(&path, &serde_json::to_string(&record).unwrap())
            .unwrap();
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
}
