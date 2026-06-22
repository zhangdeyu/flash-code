//! Integration tests for JsonlSink: concurrent writes and file integrity.

use std::sync::Arc;

use flash_code::protocol::Event;
use flash_code::sink::jsonl::JsonlSink;
use flash_code::sink::EventSink;

#[tokio::test]
async fn multiple_events_produce_valid_jsonl_lines() {
    let tmp = tempfile::NamedTempFile::new().expect("create temp file");
    let path = tmp.path().to_owned();
    let file = tokio::fs::File::create(&path).await.expect("open");
    let sink = Arc::new(JsonlSink::new(file));

    // Write several events
    for i in 0..10 {
        sink.emit(Event::AssistantToken {
            session_id: "s1".into(),
            text: format!("token_{i}"),
        })
        .await;
    }

    drop(sink);

    // Read back and verify each line is valid JSON
    let content = tokio::fs::read_to_string(&path).await.expect("read");
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 10);

    for (i, line) in lines.iter().enumerate() {
        let parsed: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line {i} is not valid JSON: {e}"));
        assert_eq!(parsed["type"], "assistant_token");
        assert_eq!(parsed["text"], format!("token_{i}"));
    }
}

#[tokio::test]
async fn concurrent_writes_do_not_corrupt_lines() {
    let tmp = tempfile::NamedTempFile::new().expect("create temp file");
    let path = tmp.path().to_owned();
    let file = tokio::fs::File::create(&path).await.expect("open");
    let sink = Arc::new(JsonlSink::new(file));

    // Spawn multiple concurrent writers
    let mut handles = Vec::new();
    for i in 0..5 {
        let sink = sink.clone();
        handles.push(tokio::spawn(async move {
            for j in 0..10 {
                sink.emit(Event::ToolStart {
                    session_id: "s1".into(),
                    call_id: format!("c{i}_{j}"),
                    tool: "bash".into(),
                    input: serde_json::json!({"command": format!("echo {i}_{j}")}),
                })
                .await;
            }
        }));
    }

    for h in handles {
        h.await.expect("task completed");
    }

    drop(sink);

    // Read back: should have 50 lines, each valid JSON
    let content = tokio::fs::read_to_string(&path).await.expect("read");
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 50);

    for (i, line) in lines.iter().enumerate() {
        let parsed: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line {i} is not valid JSON: {e}\nline content: {line}"));
        assert_eq!(parsed["type"], "tool_start");
    }
}
