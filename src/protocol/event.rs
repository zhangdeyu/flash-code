use serde::{Deserialize, Deserializer, Serialize};

/// All events emitted during a session.
///
/// Serialization uses `#[serde(tag = "type", rename_all = "snake_case")]`.
/// Deserialization is hand-written: unknown variants fall back to `Unknown` to
/// preserve forward compatibility without losing the original payload.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    SessionStarted {
        session_id: String,
    },

    AssistantMessageStart {
        session_id: String,
    },
    AssistantToken {
        session_id: String,
        text: String,
    },
    AssistantMessageEnd {
        session_id: String,
    },

    ToolStart {
        session_id: String,
        call_id: String,
        tool: String,
        input: serde_json::Value,
    },
    ToolEnd {
        session_id: String,
        call_id: String,
        output: serde_json::Value,
        duration_ms: u64,
    },
    ToolError {
        session_id: String,
        call_id: String,
        error: String,
    },
    ToolCancelled {
        session_id: String,
        call_id: String,
    },

    ApprovalRequired {
        session_id: String,
        call_id: String,
        command: String,
    },
    ApprovalGranted {
        session_id: String,
        call_id: String,
    },
    ApprovalRejected {
        session_id: String,
        call_id: String,
    },

    HistoryCompacted {
        session_id: String,
        before_count: usize,
        after_count: usize,
        summary: String,
    },

    Cancelled {
        session_id: String,
        reason: String,
    },
    Error {
        session_id: String,
        message: String,
    },

    /// Catch-all for unknown event types. Preserves the raw JSON payload.
    /// Never written by `JsonlSink`; only appears on the read/replay side.
    #[serde(skip_serializing)]
    Unknown(serde_json::Value),
}

// ---------- hand-written Deserialize (approach B: preserve raw payload) ----------

/// Internal mirror of `Event` without `Unknown`, so serde can derive for us.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum KnownEvent {
    SessionStarted { session_id: String },

    AssistantMessageStart { session_id: String },
    AssistantToken { session_id: String, text: String },
    AssistantMessageEnd { session_id: String },

    ToolStart { session_id: String, call_id: String, tool: String, input: serde_json::Value },
    ToolEnd { session_id: String, call_id: String, output: serde_json::Value, duration_ms: u64 },
    ToolError { session_id: String, call_id: String, error: String },
    ToolCancelled { session_id: String, call_id: String },

    ApprovalRequired { session_id: String, call_id: String, command: String },
    ApprovalGranted { session_id: String, call_id: String },
    ApprovalRejected { session_id: String, call_id: String },

    HistoryCompacted { session_id: String, before_count: usize, after_count: usize, summary: String },

    Cancelled { session_id: String, reason: String },
    Error { session_id: String, message: String },
}

impl From<KnownEvent> for Event {
    fn from(k: KnownEvent) -> Self {
        match k {
            KnownEvent::SessionStarted { session_id } => Self::SessionStarted { session_id },
            KnownEvent::AssistantMessageStart { session_id } => Self::AssistantMessageStart { session_id },
            KnownEvent::AssistantToken { session_id, text } => Self::AssistantToken { session_id, text },
            KnownEvent::AssistantMessageEnd { session_id } => Self::AssistantMessageEnd { session_id },
            KnownEvent::ToolStart { session_id, call_id, tool, input } => Self::ToolStart { session_id, call_id, tool, input },
            KnownEvent::ToolEnd { session_id, call_id, output, duration_ms } => Self::ToolEnd { session_id, call_id, output, duration_ms },
            KnownEvent::ToolError { session_id, call_id, error } => Self::ToolError { session_id, call_id, error },
            KnownEvent::ToolCancelled { session_id, call_id } => Self::ToolCancelled { session_id, call_id },
            KnownEvent::ApprovalRequired { session_id, call_id, command } => Self::ApprovalRequired { session_id, call_id, command },
            KnownEvent::ApprovalGranted { session_id, call_id } => Self::ApprovalGranted { session_id, call_id },
            KnownEvent::ApprovalRejected { session_id, call_id } => Self::ApprovalRejected { session_id, call_id },
            KnownEvent::HistoryCompacted { session_id, before_count, after_count, summary } => Self::HistoryCompacted { session_id, before_count, after_count, summary },
            KnownEvent::Cancelled { session_id, reason } => Self::Cancelled { session_id, reason },
            KnownEvent::Error { session_id, message } => Self::Error { session_id, message },
        }
    }
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match serde_json::from_value::<KnownEvent>(value.clone()) {
            Ok(known) => Ok(known.into()),
            Err(_) => Ok(Self::Unknown(value)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_session_started() {
        let event = Event::SessionStarted { session_id: "s1".into() };
        let json = serde_json::to_value(&event).expect("serialize");
        assert_eq!(json["type"], "session_started");
        assert_eq!(json["session_id"], "s1");
    }

    #[test]
    fn serialize_tool_start() {
        let event = Event::ToolStart {
            session_id: "s1".into(),
            call_id: "c1".into(),
            tool: "bash".into(),
            input: serde_json::json!({"command": "ls"}),
        };
        let json = serde_json::to_value(&event).expect("serialize");
        assert_eq!(json["type"], "tool_start");
        assert_eq!(json["tool"], "bash");
    }

    #[test]
    fn deserialize_known_event() {
        let json = r#"{"type":"session_started","session_id":"s1"}"#;
        let event: Event = serde_json::from_str(json).expect("deserialize");
        assert!(matches!(event, Event::SessionStarted { .. }));
    }

    #[test]
    fn deserialize_unknown_type_falls_back() {
        let json = r#"{"type":"future_event","session_id":"s1","extra":"data"}"#;
        let event: Event = serde_json::from_str(json).expect("deserialize");
        assert!(matches!(event, Event::Unknown(_)));
    }

    #[test]
    fn deserialize_with_version_field() {
        let json = r#"{"version":"1","type":"tool_start","session_id":"s1","call_id":"c1","tool":"bash","input":{"command":"ls"}}"#;
        let event: Event = serde_json::from_str(json).expect("deserialize");
        assert!(matches!(event, Event::ToolStart { .. }));
    }

    #[test]
    fn unknown_event_cannot_be_serialized() {
        let event = Event::Unknown(serde_json::json!({"type": "future"}));
        // skip_serializing causes serialization to fail, which is expected;
        // JsonlSink checks for Unknown before attempting to serialize.
        let result = serde_json::to_string(&event);
        assert!(result.is_err());
    }

    #[test]
    fn roundtrip_tool_end() {
        let event = Event::ToolEnd {
            session_id: "s1".into(),
            call_id: "c1".into(),
            output: serde_json::json!({"stdout": "hello"}),
            duration_ms: 42,
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let back: Event = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(back, Event::ToolEnd { duration_ms: 42, .. }));
    }
}
// Event enum + custom Deserialize
