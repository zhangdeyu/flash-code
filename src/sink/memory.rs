use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::protocol::Event;
use crate::sink::EventSink;

/// In-memory event collector for testing.
///
/// Uses `std::sync::Mutex<Vec<Event>>` — no `.await` in the critical section.
pub struct MemorySink {
    events: std::sync::Mutex<Vec<Event>>,
}

impl MemorySink {
    #[must_use]
    pub fn new() -> Self {
        Self {
            events: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Return a clone of all collected events.
    #[must_use]
    pub fn events(&self) -> Vec<Event> {
        self.events.lock().map_or_else(|_| Vec::new(), |e| e.clone())
    }

    /// Project events into a structural snapshot, stripping non-deterministic
    /// content (tokens, timestamps, session_ids). Used with `insta` for comparison.
    #[must_use]
    pub fn to_snapshot(&self) -> Vec<SnapshotEntry> {
        to_snapshot(&self.events())
    }

    /// Check whether a tool with the given name was called.
    #[must_use]
    pub fn tool_called(&self, tool_name: &str) -> bool {
        self.events().iter().any(|e| matches!(e, Event::ToolStart { tool, .. } if tool == tool_name))
    }

    /// Check whether any assistant message contains the given substring.
    #[must_use]
    pub fn answer_contains(&self, substring: &str) -> bool {
        self.events().iter().any(|e| {
            matches!(e, Event::AssistantToken { text, .. } if text.contains(substring))
        })
    }
}

impl Default for MemorySink {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EventSink for MemorySink {
    async fn emit(&self, event: Event) {
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        }
    }
}

// ---------- Snapshot projection ----------

/// A structural entry for snapshot testing. Only captures the "shape" of tool
/// interactions, not the content (which is non-deterministic).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SnapshotEntry {
    ToolCall {
        tool: String,
        key_args: BTreeMap<String, serde_json::Value>,
    },
    ToolSucceeded,
    ToolFailed,
    ToolCancelled,
    AssistantTurnEnd,
}

/// Extract whitelisted key arguments from a tool input based on tool name.
fn extract_whitelisted(tool: &str, input: &serde_json::Value) -> BTreeMap<String, serde_json::Value> {
    let mut map = BTreeMap::new();
    // For "bash", the key argument is "command"
    if tool == "bash" {
        if let Some(cmd) = input.get("command") {
            map.insert("command".to_owned(), cmd.clone());
        }
    }
    map
}

/// Project a sequence of events into structural snapshot entries.
pub fn to_snapshot(events: &[Event]) -> Vec<SnapshotEntry> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::ToolStart { tool, input, .. } => Some(SnapshotEntry::ToolCall {
                tool: tool.clone(),
                key_args: extract_whitelisted(tool, input),
            }),
            Event::ToolEnd { .. } => Some(SnapshotEntry::ToolSucceeded),
            Event::ToolError { .. } => Some(SnapshotEntry::ToolFailed),
            Event::ToolCancelled { .. } => Some(SnapshotEntry::ToolCancelled),
            Event::AssistantMessageEnd { .. } => Some(SnapshotEntry::AssistantTurnEnd),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn collects_events() {
        let sink = MemorySink::new();
        sink.emit(Event::SessionStarted { session_id: "s1".into() }).await;
        sink.emit(Event::AssistantToken { session_id: "s1".into(), text: "hello".into() }).await;
        assert_eq!(sink.events().len(), 2);
    }

    #[tokio::test]
    async fn tool_called_check() {
        let sink = MemorySink::new();
        sink.emit(Event::ToolStart {
            session_id: "s1".into(),
            call_id: "c1".into(),
            tool: "bash".into(),
            input: serde_json::json!({"command": "ls"}),
        }).await;
        assert!(sink.tool_called("bash"));
        assert!(!sink.tool_called("python"));
    }

    #[tokio::test]
    async fn answer_contains_check() {
        let sink = MemorySink::new();
        sink.emit(Event::AssistantToken { session_id: "s1".into(), text: "The Controller handles".into() }).await;
        assert!(sink.answer_contains("Controller"));
        assert!(!sink.answer_contains("Model"));
    }

    #[tokio::test]
    async fn to_snapshot_projects_correctly() {
        let sink = MemorySink::new();
        sink.emit(Event::AssistantMessageStart { session_id: "s1".into() }).await;
        sink.emit(Event::AssistantToken { session_id: "s1".into(), text: "I'll run ls".into() }).await;
        sink.emit(Event::AssistantMessageEnd { session_id: "s1".into() }).await;
        sink.emit(Event::ToolStart {
            session_id: "s1".into(),
            call_id: "c1".into(),
            tool: "bash".into(),
            input: serde_json::json!({"command": "ls"}),
        }).await;
        sink.emit(Event::ToolEnd {
            session_id: "s1".into(),
            call_id: "c1".into(),
            output: serde_json::json!({"stdout": "file.txt"}),
            duration_ms: 10,
        }).await;

        let snapshot = sink.to_snapshot();
        assert_eq!(snapshot.len(), 3);
        assert!(matches!(&snapshot[0], SnapshotEntry::AssistantTurnEnd));
        assert!(matches!(&snapshot[1], SnapshotEntry::ToolCall { tool, .. } if tool == "bash"));
        assert!(matches!(&snapshot[2], SnapshotEntry::ToolSucceeded));
    }
}
// MemorySink: in-memory event collection for testing
