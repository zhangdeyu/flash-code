use std::collections::BTreeSet;

use flash_core::{ContentBlock, Message, Role};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HistoryProjectionError {
    ToolMessageWithoutAssistant,
    DuplicateToolUse(String),
    OrphanToolResult(String),
    MissingToolResults(Vec<String>),
}

impl std::fmt::Display for HistoryProjectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolMessageWithoutAssistant => {
                write!(
                    formatter,
                    "history contains a tool message without an assistant"
                )
            }
            Self::DuplicateToolUse(call_id) => {
                write!(formatter, "history contains duplicate tool use `{call_id}`")
            }
            Self::OrphanToolResult(call_id) => {
                write!(formatter, "history contains orphan tool result `{call_id}`")
            }
            Self::MissingToolResults(call_ids) => {
                write!(
                    formatter,
                    "history is missing tool results for {}",
                    call_ids.join(", ")
                )
            }
        }
    }
}

pub(crate) fn project_history(
    history: &[Message],
    max_bytes: usize,
) -> Result<Vec<Message>, HistoryProjectionError> {
    let turns = conversation_turns(history)?;
    let mut projected_turns = Vec::new();
    let mut used = 0;
    for turn in turns.iter().rev() {
        let size = turn.iter().map(message_size).sum::<usize>();
        if !projected_turns.is_empty() && used + size > max_bytes {
            break;
        }
        used += size;
        projected_turns.push(turn);
    }
    projected_turns.reverse();
    let projected = projected_turns
        .into_iter()
        .flat_map(|turn| turn.iter().cloned())
        .collect::<Vec<_>>();
    validate_tool_turns(&projected)?;
    Ok(projected)
}

fn conversation_turns(history: &[Message]) -> Result<Vec<Vec<Message>>, HistoryProjectionError> {
    validate_tool_turns(history)?;
    let mut turns = Vec::<Vec<Message>>::new();
    for message in history {
        match message.role {
            Role::System | Role::User => turns.push(vec![message.clone()]),
            Role::Assistant => {
                if turns
                    .last()
                    .and_then(|turn| turn.last())
                    .is_some_and(|previous| previous.role == Role::User)
                {
                    if let Some(turn) = turns.last_mut() {
                        turn.push(message.clone());
                    }
                } else {
                    turns.push(vec![message.clone()]);
                }
            }
            Role::Tool => {
                let Some(turn) = turns.last_mut() else {
                    return Err(HistoryProjectionError::ToolMessageWithoutAssistant);
                };
                if !turn
                    .iter()
                    .any(|candidate| candidate.role == Role::Assistant)
                {
                    return Err(HistoryProjectionError::ToolMessageWithoutAssistant);
                }
                turn.push(message.clone());
            }
        }
    }
    Ok(turns)
}

pub(crate) fn validate_tool_turns(history: &[Message]) -> Result<(), HistoryProjectionError> {
    let mut pending = BTreeSet::new();
    for message in history {
        match message.role {
            Role::Assistant => {
                if !pending.is_empty() {
                    return Err(HistoryProjectionError::MissingToolResults(
                        pending.into_iter().collect(),
                    ));
                }
                for block in &message.content {
                    if let ContentBlock::ToolUse { call_id, .. } = block {
                        if !pending.insert(call_id.clone()) {
                            return Err(HistoryProjectionError::DuplicateToolUse(call_id.clone()));
                        }
                    }
                }
            }
            Role::Tool => {
                for block in &message.content {
                    if let ContentBlock::ToolResult { call_id, .. } = block {
                        if !pending.remove(call_id) {
                            return Err(HistoryProjectionError::OrphanToolResult(call_id.clone()));
                        }
                    }
                }
            }
            Role::System | Role::User => {
                if !pending.is_empty() {
                    return Err(HistoryProjectionError::MissingToolResults(
                        pending.into_iter().collect(),
                    ));
                }
            }
        }
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err(HistoryProjectionError::MissingToolResults(
            pending.into_iter().collect(),
        ))
    }
}

fn message_size(message: &Message) -> usize {
    message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } | ContentBlock::Reasoning { text } => text.len(),
            ContentBlock::ToolUse {
                call_id,
                name,
                input,
            } => call_id.len() + name.len() + input.to_string().len(),
            ContentBlock::ToolResult { call_id, .. } => call_id.len(),
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_message;
    use flash_core::{ContentBlock, Role, ToolResultStatus};
    use serde_json::json;

    #[test]
    fn project_history_should_keep_recent_messages_within_budget() {
        let old = test_message("old text");
        let recent = test_message("new");

        let projected = project_history(&[old, recent.clone()], 3).unwrap();

        assert_eq!(projected, vec![recent]);
    }

    #[test]
    fn project_history_should_keep_complete_tool_turn_when_budget_is_tiny() {
        let old = test_message("old");
        let user = test_message("task");
        let assistant = Message {
            id: "assistant".to_string(),
            role: Role::Assistant,
            created_at: "0".to_string(),
            content: vec![ContentBlock::ToolUse {
                call_id: "call_1".to_string(),
                name: "Read".to_string(),
                input: json!({"path": "README.md"}),
            }],
        };
        let tool = Message {
            id: "tool".to_string(),
            role: Role::Tool,
            created_at: "0".to_string(),
            content: vec![
                ContentBlock::ToolResult {
                    call_id: "call_1".to_string(),
                    status: ToolResultStatus::Success,
                },
                ContentBlock::Text {
                    text: "result".to_string(),
                },
            ],
        };

        let projected =
            project_history(&[old, user.clone(), assistant.clone(), tool.clone()], 1).unwrap();

        assert_eq!(projected, vec![user, assistant, tool]);
    }

    #[test]
    fn project_history_should_reject_orphan_tool_result() {
        let tool = Message {
            id: "tool".to_string(),
            role: Role::Tool,
            created_at: "0".to_string(),
            content: vec![ContentBlock::ToolResult {
                call_id: "call_1".to_string(),
                status: ToolResultStatus::Success,
            }],
        };

        let error = project_history(&[tool], 100).unwrap_err();

        assert_eq!(
            error,
            HistoryProjectionError::OrphanToolResult("call_1".to_string())
        );
    }
}
