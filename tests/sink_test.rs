use flash_code::protocol::{Compaction, CompactionTrigger, ContentBlock, Event, Message};
use flash_code::provider::Usage;

#[test]
fn message_appended_round_trips() {
    let m = Message::user_text("hi");
    let event = Event::MessageAppended {
        session_id: "s1".into(),
        message: m,
    };
    let json = serde_json::to_string(&event).expect("serialize");
    let back: Event = serde_json::from_str(&json).expect("deserialize");
    assert!(matches!(back, Event::MessageAppended { .. }));
}

#[test]
fn history_compacted_serializes_with_compaction() {
    let c = Compaction::new("summary".into(), "msg-id".into(), CompactionTrigger::Auto);
    let event = Event::HistoryCompacted {
        session_id: "s1".into(),
        compaction: c,
        before_count: 10,
        tail_count: 4,
    };
    let json = serde_json::to_value(&event).expect("serialize");
    assert_eq!(json["type"], "history_compacted");
    assert_eq!(json["before_count"], 10);
    assert_eq!(json["tail_count"], 4);
    assert!(json["compaction"]["summary"].as_str().is_some());
}

#[test]
fn micro_compacted_serializes() {
    let event = Event::MicroCompacted {
        session_id: "s1".into(),
        redacted_ids: vec!["c1".into(), "c2".into()],
        bytes_saved: 4096,
    };
    let json = serde_json::to_value(&event).expect("serialize");
    assert_eq!(json["type"], "micro_compacted");
    assert_eq!(json["bytes_saved"], 4096);
    assert_eq!(json["redacted_ids"][0], "c1");
}

#[test]
fn unknown_event_falls_back() {
    let json = r#"{"type":"future_event","payload":42}"#;
    let event: Event = serde_json::from_str(json).expect("deserialize");
    assert!(matches!(event, Event::Unknown(_)));
}

#[test]
fn tool_start_round_trips() {
    let event = Event::ToolStart {
        session_id: "s1".into(),
        call_id: "c1".into(),
        tool: "bash".into(),
        input: serde_json::json!({"command": "ls"}),
    };
    let json = serde_json::to_string(&event).expect("serialize");
    let back: Event = serde_json::from_str(&json).expect("deserialize");
    assert!(matches!(back, Event::ToolStart { .. }));
}

#[test]
fn usage_event_round_trips() {
    let event = Event::Usage {
        session_id: "s1".into(),
        usage: Usage {
            prompt_tokens: 100,
            completion_tokens: 200,
            total_tokens: 300,
            reasoning_tokens: Some(150),
        },
    };
    let json = serde_json::to_value(&event).expect("serialize");
    assert_eq!(json["type"], "usage");
    assert_eq!(json["usage"]["prompt_tokens"], 100);
    assert_eq!(json["usage"]["reasoning_tokens"], 150);
    let back: Event = serde_json::from_value(json).expect("deserialize");
    assert!(matches!(back, Event::Usage { .. }));
}

#[test]
fn user_message_blocks_serializes_with_id() {
    // Confirm Message round-trips include id and role
    let m = Message::user(vec![ContentBlock::text("hi")]);
    let json = serde_json::to_value(&m).expect("serialize");
    assert_eq!(json["role"], "user");
    assert!(json["id"].as_str().is_some());
    let back: Message = serde_json::from_value(json).expect("deserialize");
    assert_eq!(back.role, m.role);
}
