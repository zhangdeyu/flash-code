use flash_code::protocol::{
    apply_micro_compact, Compaction, CompactionTrigger, ContentBlock, History, HistoryError,
    MicroCompactPolicy, Role, MICRO_PLACEHOLDER,
};

fn user_block(text: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::text(text)]
}

#[test]
fn push_user_records_message() {
    let mut h = History::new();
    let id = h.push_user(user_block("hi"));
    assert!(!id.is_empty());
    assert_eq!(h.raw_messages().len(), 1);
    assert_eq!(h.raw_messages()[0].role, Role::User);
}

#[test]
fn push_tool_results_pairing_ok() {
    let mut h = History::new();
    h.push_user(user_block("go"));
    h.push_assistant(vec![
        ContentBlock::ToolUse {
            call_id: "c1".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        },
        ContentBlock::ToolUse {
            call_id: "c2".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        },
    ]);
    let r1 = ContentBlock::tool_result("c1", vec![ContentBlock::text("a")], false).unwrap();
    let r2 = ContentBlock::tool_result("c2", vec![ContentBlock::text("b")], false).unwrap();
    // Reverse order is fine — set equality, not ordering.
    let res = h.push_tool_results(vec![r2, r1]);
    assert!(res.is_ok());
}

#[test]
fn push_tool_results_mismatch_errors() {
    let mut h = History::new();
    h.push_user(user_block("go"));
    h.push_assistant(vec![ContentBlock::ToolUse {
        call_id: "c1".into(),
        name: "bash".into(),
        input: serde_json::json!({}),
    }]);
    let bad = ContentBlock::tool_result("c2", vec![ContentBlock::text("x")], false).unwrap();
    let r = h.push_tool_results(vec![bad]);
    assert!(matches!(r, Err(HistoryError::ToolResultMismatch { .. })));
}

#[test]
fn record_compaction_tail_not_found() {
    let mut h = History::new();
    h.push_user(user_block("a"));
    h.push_assistant(vec![ContentBlock::text("b")]);
    h.push_user(user_block("c"));
    h.push_assistant(vec![ContentBlock::text("d")]);
    let c = Compaction::new("s".into(), "no-such-id".into(), CompactionTrigger::Auto);
    assert!(matches!(
        h.record_compaction(c),
        Err(HistoryError::TailNotFound(_))
    ));
}

#[test]
fn record_compaction_tail_must_be_turn_boundary() {
    let mut h = History::new();
    h.push_user(user_block("a"));
    h.push_assistant(vec![ContentBlock::text("b")]);
    h.push_user(user_block("c"));
    h.push_assistant(vec![ContentBlock::text("d")]);

    let asst_id = h.raw_messages()[1].id.clone();
    let c = Compaction::new("s".into(), asst_id, CompactionTrigger::Auto);
    assert!(matches!(
        h.record_compaction(c),
        Err(HistoryError::TailNotOnTurnBoundary(_))
    ));
}

#[test]
fn record_compaction_empty_tail() {
    let mut h = History::new();
    h.push_user(user_block("a"));
    h.push_assistant(vec![ContentBlock::text("b")]);
    h.push_user(user_block("c"));
    let last = h.raw_messages().last().unwrap().id.clone();
    let c = Compaction::new("s".into(), last, CompactionTrigger::Auto);
    assert!(matches!(
        h.record_compaction(c),
        Err(HistoryError::EmptyTail)
    ));
}

#[test]
fn record_compaction_monotonic() {
    let mut h = History::new();
    let u1 = h.push_user(user_block("a"));
    h.push_assistant(vec![ContentBlock::text("b")]);
    let _u2 = h.push_user(user_block("c"));
    h.push_assistant(vec![ContentBlock::text("d")]);
    let _u3 = h.push_user(user_block("e"));
    h.push_assistant(vec![ContentBlock::text("f")]);

    // Take user2 as tail
    let user2_id = h.raw_messages()[2].id.clone();
    let c1 = Compaction::new("s1".into(), user2_id, CompactionTrigger::Auto);
    h.record_compaction(c1).unwrap();

    // Going back to user1 must fail (non-monotonic)
    let c2 = Compaction::new("s2".into(), u1, CompactionTrigger::Auto);
    assert!(matches!(
        h.record_compaction(c2),
        Err(HistoryError::NonMonotonicCompaction { .. })
    ));
}

#[test]
fn projection_no_compaction_returns_clone() {
    let mut h = History::new();
    h.push_user(user_block("hi"));
    h.push_assistant(vec![ContentBlock::text("yo")]);
    let p = h.to_prompt_messages();
    assert_eq!(p.len(), 2);
    assert_eq!(p[0].role, Role::User);
    assert_eq!(p[1].role, Role::Assistant);
}

#[test]
fn projection_with_compaction_prepends_summary() {
    let mut h = History::new();
    h.push_user(user_block("a"));
    h.push_assistant(vec![ContentBlock::text("b")]);
    h.push_user(user_block("c"));
    h.push_assistant(vec![ContentBlock::text("d")]);
    let user2_id = h.raw_messages()[2].id.clone();
    let c = Compaction::new("SUMMARY".into(), user2_id, CompactionTrigger::Auto);
    h.record_compaction(c).unwrap();
    let p = h.to_prompt_messages();
    assert_eq!(p[0].role, Role::Summary);
    assert_eq!(p[0].first_text(), Some("SUMMARY"));
    // followed by user2, assistant2
    assert_eq!(p[1].role, Role::User);
    assert_eq!(p[2].role, Role::Assistant);
}

#[test]
fn reasoning_dropped_before_compaction_kept_in_tail() {
    let mut h = History::new();
    // turn 1 with reasoning
    h.push_user(user_block("u1"));
    h.push_assistant(vec![
        ContentBlock::Reasoning {
            text: "thinking-1".into(),
            signature: None,
        },
        ContentBlock::text("a1"),
    ]);
    // turn 2 with reasoning
    h.push_user(user_block("u2"));
    h.push_assistant(vec![
        ContentBlock::Reasoning {
            text: "thinking-2".into(),
            signature: Some("sig".into()),
        },
        ContentBlock::text("a2"),
    ]);

    // Compact at user2
    let user2_id = h.raw_messages()[2].id.clone();
    let c = Compaction::new("S".into(), user2_id, CompactionTrigger::Auto);
    h.record_compaction(c).unwrap();

    let p = h.to_prompt_messages();
    // Summary + user2 + assistant2 (with reasoning)
    let asst_tail = p.iter().find(|m| m.role == Role::Assistant).unwrap();
    let has_reason = asst_tail
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::Reasoning { .. }));
    assert!(has_reason, "tail assistant must keep reasoning");
}

#[test]
fn projection_idempotent() {
    let mut h = History::new();
    h.push_user(user_block("x"));
    h.push_assistant(vec![ContentBlock::text("y")]);
    let p1 = h.to_prompt_messages();
    let p2 = h.to_prompt_messages();
    assert_eq!(p1.len(), p2.len());
    for (a, b) in p1.iter().zip(p2.iter()) {
        assert_eq!(a.role, b.role);
    }
}

// ---- MicroCompact ----

fn build_history_with_n_tool_results(n: usize, big: bool) -> History {
    let mut h = History::new();
    for i in 0..n {
        h.push_user(user_block(&format!("u{i}")));
        let call_id = format!("c{i}");
        h.push_assistant(vec![ContentBlock::ToolUse {
            call_id: call_id.clone(),
            name: "bash".into(),
            input: serde_json::json!({}),
        }]);
        let body = if big { "x".repeat(8_000) } else { "ok".to_owned() };
        let r = ContentBlock::tool_result(&call_id, vec![ContentBlock::text(body)], false).unwrap();
        h.push_tool_results(vec![r]).unwrap();
    }
    h
}

#[test]
fn micro_compact_redacts_old_large_results() {
    let h = build_history_with_n_tool_results(10, true);
    let policy = MicroCompactPolicy::default(); // stale=4, keep_recent=5, size=4000
    let mut projected = h.to_prompt_messages();
    // run again on a fresh slice (the previous projection already applied micro)
    let mut fresh = h.compact_slice();
    let r = apply_micro_compact(&mut fresh, &policy);
    assert!(!r.redacted_ids.is_empty(), "should redact some old results");
    assert!(r.bytes_saved > 0);
    // raw messages untouched
    assert_eq!(h.raw_messages().len(), projected.len());
    // verify placeholder content
    let any_placeholder = fresh.iter().any(|m| {
        m.content.iter().any(|b| matches!(b, ContentBlock::ToolResult { content, .. }
            if content.iter().any(|x| matches!(x, ContentBlock::Text { text } if text == MICRO_PLACEHOLDER))))
    });
    assert!(any_placeholder);
    // also: projected (already micro'd) is idempotent
    let r2 = apply_micro_compact(&mut projected, &policy);
    assert_eq!(r2.redacted_ids.len(), 0);
}

#[test]
fn micro_compact_does_not_redact_small_old_results() {
    let h = build_history_with_n_tool_results(10, false);
    let policy = MicroCompactPolicy::default();
    let mut fresh = h.compact_slice();
    let r = apply_micro_compact(&mut fresh, &policy);
    assert!(r.redacted_ids.is_empty(), "small results never redacted");
}

#[test]
fn replay_via_message_appended_zero_llm() {
    // Simulate appending raw messages reconstructs an equivalent history (size-wise).
    let mut h = History::new();
    h.push_user(user_block("a"));
    h.push_assistant(vec![ContentBlock::text("b")]);
    h.push_user(user_block("c"));
    h.push_assistant(vec![ContentBlock::text("d")]);

    let snapshot: Vec<_> = h.raw_messages().to_vec();
    let mut h2 = History::new();
    for m in snapshot {
        // Cannot call private push_message_unchecked from outside crate; rely on public push_*
        match m.role {
            Role::User => {
                h2.push_user(m.content);
            }
            Role::Assistant => {
                h2.push_assistant(m.content);
            }
            Role::Tool => {
                let _ = h2.push_tool_results(m.content);
            }
            _ => {}
        }
    }
    assert_eq!(h.raw_messages().len(), h2.raw_messages().len());
}
