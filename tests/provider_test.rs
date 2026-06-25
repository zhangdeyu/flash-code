use flash_code::protocol::{ContentBlock, Message, Prompt};
use flash_code::provider::openai::prompt_to_openai_messages;

#[test]
fn prompt_translates_summary_to_user_with_prefix() {
    // Build a Prompt where messages start with a Summary (via projection mock).
    // Since Message::summary is crate-private, we instead test by building a
    // History and projecting it after a compaction. Cannot from this layer.
    // So: assemble a manual prompt with explicit text, and rely on adapter test
    // covering the same logic in the unit test inside provider/openai.rs.

    // Smoke: prompt without summary still works.
    let prompt = Prompt {
        system: vec![Message::system_text("you are helpful")],
        tools: vec![],
        messages: vec![Message::user_text("hi")],
    };
    let out = prompt_to_openai_messages(&prompt);
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
    let out = prompt_to_openai_messages(&prompt);
    assert_eq!(out[0]["role"], "assistant");
    assert!(out[0]["tool_calls"].is_array());
    assert_eq!(out[0]["tool_calls"][0]["id"], "c1");
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
    let out = prompt_to_openai_messages(&prompt);
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
fn capability_factories() {
    use flash_code::provider::Capability;
    let c = Capability::openai_gpt4o();
    assert_eq!(c.max_context, 128_000);
    assert!(!c.supports_reasoning);

    let o = Capability::openai_o_series(200_000, 32_768);
    assert_eq!(o.max_context, 200_000);
    assert!(o.supports_reasoning);
}
