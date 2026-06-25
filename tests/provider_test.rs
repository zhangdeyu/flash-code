use flash_code::protocol::{ContentBlock, Message, Prompt};
use flash_code::provider::deepseek::prompt_to_deepseek_messages;

#[test]
fn prompt_translates_summary_to_user_with_prefix() {
    // Smoke: prompt without summary still works.
    let prompt = Prompt {
        system: vec![Message::system_text("you are helpful")],
        tools: vec![],
        messages: vec![Message::user_text("hi")],
    };
    let out = prompt_to_deepseek_messages(&prompt);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0]["role"], "system");
    assert_eq!(out[1]["role"], "user");
}

#[test]
fn assistant_with_tool_use_has_tool_calls_array() {
    let asst = Message::assistant(vec![
        ContentBlock::text("calling..."),
        ContentBlock::ToolUse {
            call_id: "c1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "ls"}),
        },
    ]);
    let prompt = Prompt {
        system: vec![],
        tools: vec![],
        messages: vec![asst],
    };
    let out = prompt_to_deepseek_messages(&prompt);
    assert_eq!(out[0]["role"], "assistant");
    assert!(out[0]["tool_calls"].is_array());
    assert_eq!(out[0]["tool_calls"][0]["id"], "c1");
}

#[test]
fn assistant_reasoning_is_replayed_as_reasoning_content() {
    let asst = Message::assistant(vec![
        ContentBlock::Reasoning {
            text: "let me think".into(),
            signature: None,
        },
        ContentBlock::ToolUse {
            call_id: "c1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "pwd"}),
        },
    ]);
    let prompt = Prompt {
        system: vec![],
        tools: vec![],
        messages: vec![asst],
    };
    let out = prompt_to_deepseek_messages(&prompt);
    assert_eq!(out[0]["reasoning_content"], "let me think");
    assert!(out[0]["tool_calls"].is_array());
}

#[test]
fn tool_message_expands_to_multiple_role_tool_messages() {
    let r1 = ContentBlock::tool_result("c1", vec![ContentBlock::text("a")], false).unwrap();
    let r2 = ContentBlock::tool_result("c2", vec![ContentBlock::text("b")], true).unwrap();
    let msg = Message::tool_results(vec![r1, r2]);
    let prompt = Prompt {
        system: vec![],
        tools: vec![],
        messages: vec![msg],
    };
    let out = prompt_to_deepseek_messages(&prompt);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0]["role"], "tool");
    assert_eq!(out[0]["tool_call_id"], "c1");
    assert_eq!(out[1]["role"], "tool");
    assert_eq!(out[1]["tool_call_id"], "c2");
}

#[test]
fn provider_error_is_retryable() {
    use flash_code::provider::ProviderError;
    use std::time::Duration;
    assert!(ProviderError::Transient("net".into()).is_retryable());
    assert!(ProviderError::RateLimited {
        retry_after: Some(Duration::from_secs(1)),
        message: "slow".into()
    }
    .is_retryable());
    assert!(!ProviderError::Auth("bad key".into()).is_retryable());
    assert!(!ProviderError::ContextOverflow("too long".into()).is_retryable());
    assert!(!ProviderError::InvalidRequest("bug".into()).is_retryable());
}

#[test]
fn capability_factory() {
    use flash_code::provider::Capability;
    let c = Capability::deepseek_v4_pro();
    assert_eq!(c.max_context, 128_000);
    assert_eq!(c.max_output, 8_192);
}
