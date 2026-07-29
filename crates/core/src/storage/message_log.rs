use std::fs;

use serde_json::Value;

use crate::protocol::{ContentBlock, Event, Message, Role, ToolResultStatus};

use super::event_log::append_event;
use super::session::{touch_session, Session};
use super::{append_line, new_id, run_blocking_storage, timestamp, MessageRecord, StorageError};

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

pub fn load_session_messages(session: &Session) -> Result<Vec<Message>, StorageError> {
    let path = session.path.join("messages.jsonl");
    super::recovery::repair_jsonl::<MessageRecord>(&path, session.limits.max_jsonl_bytes)?;
    let content = fs::read_to_string(path)?;
    content
        .lines()
        .map(|line| {
            let record: MessageRecord = serde_json::from_str(line)?;
            Ok(record.message)
        })
        .collect()
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

fn message_to_jsonl(message: &Message) -> Result<String, StorageError> {
    Ok(serde_json::to_string(&MessageRecord {
        version: "1".to_string(),
        message: message.clone(),
    })?)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::protocol::ToolResultStatus;

    #[test]
    fn append_user_message_should_write_message_and_event() {
        let root = super::super::test_support::temp_dir("append_user");
        fs::create_dir_all(&root).unwrap();
        let session = super::super::session::create_session(&root).unwrap();

        append_user_message(&session, "hello").unwrap();

        let messages = fs::read_to_string(session.path.join("messages.jsonl")).unwrap();
        assert!(messages.contains("\"role\":\"user\""));
    }

    #[test]
    fn message_schema_should_match_golden() {
        let root = super::super::test_support::temp_dir("message_schema_golden");
        let session = super::super::session::test_session(&root, "session_messages");

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
        let normalized =
            super::super::test_support::normalize_json_string_field(&actual, "id", "<message_id>");
        let normalized = super::super::test_support::normalize_json_string_field(
            &normalized,
            "created_at",
            "<timestamp>",
        );

        assert_eq!(
            normalized,
            include_str!("../../tests/golden/messages_v1.jsonl")
        );
    }

    #[test]
    fn message_jsonl_limit_should_stop_before_partial_write() {
        let root = super::super::test_support::temp_dir("message_jsonl_limit");
        fs::create_dir_all(&root).unwrap();
        let mut session = super::super::session::create_session(&root).unwrap();
        session.limits.max_jsonl_bytes = 32;

        let error = append_user_message(&session, &"x".repeat(128)).unwrap_err();

        assert!(matches!(error, StorageError::ResourceLimit { .. }));
        assert!(fs::read_to_string(session.path.join("messages.jsonl"))
            .unwrap()
            .is_empty());
    }
}
