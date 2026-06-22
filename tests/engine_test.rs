//! Integration tests for the engine run_loop using MockProvider + MemorySink.

use std::sync::Arc;

use futures::stream::{self, BoxStream};
use tokio::sync::mpsc;

use flash_code::engine::run_loop::{run_loop, ApprovalMode};
use flash_code::error::Result;
use flash_code::protocol::{Event, Message, Prompt, Role};
use flash_code::provider::{ModelEvent, Provider};
use flash_code::session::Session;
use flash_code::sink::memory::MemorySink;
use flash_code::tool::bash::BashTool;

// ---------- Mock Provider ----------

/// A mock provider that returns a predefined sequence of ModelEvents.
struct MockProvider {
    events: Vec<ModelEvent>,
}

impl MockProvider {
    fn text_only(text: &str) -> Self {
        Self {
            events: vec![ModelEvent::Token(text.to_owned()), ModelEvent::Done],
        }
    }

    fn with_tool_call(text: &str, call_id: &str, name: &str, input: serde_json::Value) -> Self {
        Self {
            events: vec![
                ModelEvent::Token(text.to_owned()),
                ModelEvent::ToolUse {
                    call_id: call_id.to_owned(),
                    name: name.to_owned(),
                    input,
                },
                ModelEvent::Done,
            ],
        }
    }

    fn multi_turn(turns: Vec<Vec<ModelEvent>>) -> MultiTurnProvider {
        MultiTurnProvider {
            turns: std::sync::Mutex::new(turns.into_iter().collect()),
        }
    }
}

#[async_trait::async_trait]
impl Provider for MockProvider {
    async fn stream(&self, _prompt: &Prompt) -> BoxStream<'_, ModelEvent> {
        Box::pin(stream::iter(self.events.clone()))
    }

    async fn complete_once(&self, _prompt: &Prompt) -> Result<String> {
        Ok("summary of conversation".to_owned())
    }
}

/// Multi-turn mock provider that returns different events on each call.
struct MultiTurnProvider {
    turns: std::sync::Mutex<std::collections::VecDeque<Vec<ModelEvent>>>,
}

#[async_trait::async_trait]
impl Provider for MultiTurnProvider {
    async fn stream(&self, _prompt: &Prompt) -> BoxStream<'_, ModelEvent> {
        let events = {
            let mut turns = self.turns.lock().unwrap_or_else(|e| e.into_inner());
            turns.pop_front().unwrap_or_else(|| vec![ModelEvent::Done])
        };
        Box::pin(stream::iter(events))
    }

    async fn complete_once(&self, _prompt: &Prompt) -> Result<String> {
        Ok("compacted summary".to_owned())
    }
}

// ---------- Tests ----------

#[tokio::test]
async fn text_only_response_no_tool_calls() {
    let sink = Arc::new(MemorySink::new());
    let mut session = Session::new("s1".into(), vec![], sink.clone());
    session.history.push(Message::user("say hello"));

    let provider = MockProvider::text_only("Hello! How can I help?");
    let tools: Vec<Box<dyn flash_code::tool::Tool>> = vec![];
    let (_tx, mut rx) = mpsc::channel::<bool>(1);

    let result = run_loop(&mut session, ApprovalMode::Yolo, &provider, &tools, &mut rx).await;
    assert!(result.is_ok());

    // Should have AssistantMessageStart, Token, AssistantMessageEnd
    assert!(sink.answer_contains("Hello!"));
    let snapshot = sink.to_snapshot();
    assert_eq!(snapshot.len(), 1); // Just AssistantTurnEnd
    assert!(matches!(
        &snapshot[0],
        flash_code::sink::memory::SnapshotEntry::AssistantTurnEnd
    ));
}

#[tokio::test]
async fn single_tool_call_yolo_mode() {
    let sink = Arc::new(MemorySink::new());
    let mut session = Session::new("s1".into(), vec![], sink.clone());
    session.history.push(Message::user("list files"));

    // First turn: model requests bash tool
    // Second turn: model produces final text answer
    let provider = MockProvider::multi_turn(vec![
        vec![
            ModelEvent::Token("Let me check.".to_owned()),
            ModelEvent::ToolUse {
                call_id: "c1".to_owned(),
                name: "bash".to_owned(),
                input: serde_json::json!({"command": "echo test_file.txt"}),
            },
            ModelEvent::Done,
        ],
        vec![
            ModelEvent::Token("Here are the files: test_file.txt".to_owned()),
            ModelEvent::Done,
        ],
    ]);

    let tools: Vec<Box<dyn flash_code::tool::Tool>> = vec![Box::new(BashTool)];
    let (_tx, mut rx) = mpsc::channel::<bool>(1);

    let result = run_loop(&mut session, ApprovalMode::Yolo, &provider, &tools, &mut rx).await;
    assert!(result.is_ok());

    // Verify tool was called
    assert!(sink.tool_called("bash"));
    assert!(sink.answer_contains("test_file.txt"));

    // Snapshot should have: AssistantTurnEnd, ToolCall, ToolSucceeded, AssistantTurnEnd
    let snapshot = sink.to_snapshot();
    assert!(snapshot.len() >= 3);
}

#[tokio::test]
async fn cancel_during_streaming() {
    let sink = Arc::new(MemorySink::new());
    let mut session = Session::new("s1".into(), vec![], sink.clone());
    session.history.push(Message::user("hello"));

    // Cancel immediately before model call
    session.cancel.cancel();

    let provider = MockProvider::text_only("should not appear");
    let tools: Vec<Box<dyn flash_code::tool::Tool>> = vec![];
    let (_tx, mut rx) = mpsc::channel::<bool>(1);

    let result = run_loop(&mut session, ApprovalMode::Yolo, &provider, &tools, &mut rx).await;
    assert!(result.is_ok());

    // Should have a Cancelled event
    let events = sink.events();
    let has_cancelled = events
        .iter()
        .any(|e| matches!(e, Event::Cancelled { .. }));
    assert!(has_cancelled);

    // History should still be clean (only the user message we pushed)
    assert_eq!(session.history.len(), 1);
}

#[tokio::test]
async fn cancel_during_approval_backfills_tool_results() {
    let sink = Arc::new(MemorySink::new());
    let mut session = Session::new("s1".into(), vec![], sink.clone());
    session.history.push(Message::user("do something"));

    let provider = MockProvider::with_tool_call(
        "I'll run a command",
        "c1",
        "bash",
        serde_json::json!({"command": "ls"}),
    );

    let tools: Vec<Box<dyn flash_code::tool::Tool>> = vec![Box::new(BashTool)];
    let (_tx, mut rx) = mpsc::channel::<bool>(1);

    // Cancel while waiting for approval
    let cancel = session.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.cancel();
    });

    let result = run_loop(
        &mut session,
        ApprovalMode::Default,
        &provider,
        &tools,
        &mut rx,
    )
    .await;
    assert!(result.is_ok());

    // History should have: user msg + assistant msg + tool_result (backfilled)
    assert_eq!(session.history.len(), 3);
    let last = &session.history[2];
    assert_eq!(last.role, Role::Tool);
    assert!(last.is_error);
}

#[tokio::test]
async fn max_turns_exceeded() {
    let sink = Arc::new(MemorySink::new());
    let mut session = Session::new("s1".into(), vec![], sink.clone());
    session.history.push(Message::user("infinite loop"));

    // Provider always returns a tool call, creating infinite loop
    struct InfiniteToolProvider;

    #[async_trait::async_trait]
    impl Provider for InfiniteToolProvider {
        async fn stream(&self, _prompt: &Prompt) -> BoxStream<'_, ModelEvent> {
            Box::pin(stream::iter(vec![
                ModelEvent::Token("again".to_owned()),
                ModelEvent::ToolUse {
                    call_id: "cx".to_owned(),
                    name: "bash".to_owned(),
                    input: serde_json::json!({"command": "echo loop"}),
                },
                ModelEvent::Done,
            ]))
        }

        async fn complete_once(&self, _prompt: &Prompt) -> Result<String> {
            Ok("summary".to_owned())
        }
    }

    let tools: Vec<Box<dyn flash_code::tool::Tool>> = vec![Box::new(BashTool)];
    let (_tx, mut rx) = mpsc::channel::<bool>(1);

    let result = run_loop(
        &mut session,
        ApprovalMode::Yolo,
        &InfiniteToolProvider,
        &tools,
        &mut rx,
    )
    .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.to_string(), "max turns exceeded");
}

#[tokio::test]
async fn tool_call_rejected() {
    let sink = Arc::new(MemorySink::new());
    let mut session = Session::new("s1".into(), vec![], sink.clone());
    session.history.push(Message::user("run dangerous command"));

    // First turn: model requests tool, gets rejected, then gives text answer
    let provider = MockProvider::multi_turn(vec![
        vec![
            ModelEvent::Token("Let me run rm -rf".to_owned()),
            ModelEvent::ToolUse {
                call_id: "c1".to_owned(),
                name: "bash".to_owned(),
                input: serde_json::json!({"command": "rm -rf /"}),
            },
            ModelEvent::Done,
        ],
        // After rejection, model responds with text
        vec![
            ModelEvent::Token("Understood, I won't do that.".to_owned()),
            ModelEvent::Done,
        ],
    ]);

    let tools: Vec<Box<dyn flash_code::tool::Tool>> = vec![Box::new(BashTool)];
    let (tx, mut rx) = mpsc::channel::<bool>(1);

    // Send rejection
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let _ = tx.send(false).await;
    });

    let result = run_loop(
        &mut session,
        ApprovalMode::Default,
        &provider,
        &tools,
        &mut rx,
    )
    .await;
    assert!(result.is_ok());

    // History should contain rejection tool_result
    let rejected = session.history.iter().any(|m| {
        m.role == Role::Tool && m.is_error && m.content.contains("rejected")
    });
    assert!(rejected);
}
