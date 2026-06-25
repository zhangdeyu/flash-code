use std::sync::Arc;
use std::time::Duration;

use flash_code::protocol::{ApprovalDecision, RiskLevel, ToolApprovalAdvice};
use flash_code::sink::memory::MemorySink;
use flash_code::tool::bash::BashTool;
use flash_code::tool::{
    ApprovalGate, ApprovalMode, ApprovalOutcome, ExecutionContext, Tool, ToolRegistry,
};
use tokio_util::sync::CancellationToken;

#[test]
fn registry_register_and_get() {
    let mut r = ToolRegistry::new();
    r.register(Arc::new(BashTool));
    assert!(r.get("bash").is_some());
    assert!(r.get("nonexistent").is_none());
}

#[test]
#[should_panic(expected = "duplicate tool name")]
fn registry_duplicate_panics() {
    let mut r = ToolRegistry::new();
    r.register(Arc::new(BashTool));
    r.register(Arc::new(BashTool));
}

#[test]
fn registry_specs_collects_all() {
    let mut r = ToolRegistry::new();
    r.register(Arc::new(BashTool));
    let specs = r.specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].name, "bash");
}

#[test]
fn approval_gate_yolo_default_auto() {
    let g = ApprovalGate {
        mode: ApprovalMode::Yolo,
    };
    let out = g.decide(
        RiskLevel::Dangerous,
        ToolApprovalAdvice::default_for(RiskLevel::Dangerous),
    );
    assert_eq!(out, ApprovalOutcome::AutoApprove);
}

#[test]
fn approval_gate_yolo_must_ask_overrides() {
    let g = ApprovalGate {
        mode: ApprovalMode::Yolo,
    };
    let out = g.decide(RiskLevel::Safe, ToolApprovalAdvice::must_ask("danger"));
    assert_eq!(out, ApprovalOutcome::Ask);
}

#[test]
fn approval_gate_default_safe_auto() {
    let g = ApprovalGate {
        mode: ApprovalMode::Default,
    };
    let out = g.decide(
        RiskLevel::Safe,
        ToolApprovalAdvice::default_for(RiskLevel::Safe),
    );
    assert_eq!(out, ApprovalOutcome::AutoApprove);
}

#[test]
fn approval_gate_default_dangerous_asks() {
    let g = ApprovalGate {
        mode: ApprovalMode::Default,
    };
    let out = g.decide(
        RiskLevel::Dangerous,
        ToolApprovalAdvice::default_for(RiskLevel::Dangerous),
    );
    assert_eq!(out, ApprovalOutcome::Ask);
}

#[test]
fn approval_gate_default_auto_advice_overrides() {
    let g = ApprovalGate {
        mode: ApprovalMode::Default,
    };
    let out = g.decide(
        RiskLevel::Dangerous,
        ToolApprovalAdvice::auto_approve("read-only"),
    );
    assert_eq!(out, ApprovalOutcome::AutoApprove);
}

#[test]
fn bash_rm_rf_returns_must_ask() {
    let t = BashTool;
    let advice = t.approval_advice(&serde_json::json!({"command": "rm -rf /tmp/x"}));
    assert_eq!(advice.decision, ApprovalDecision::MustAsk);
}

#[test]
fn bash_normal_command_uses_default() {
    let t = BashTool;
    let advice = t.approval_advice(&serde_json::json!({"command": "ls"}));
    assert_eq!(advice.decision, ApprovalDecision::Default);
}

#[tokio::test]
async fn bash_executes_simple_command() {
    let t = BashTool;
    let ctx = ExecutionContext {
        session_id: "s1".into(),
        call_id: "c1".into(),
        cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        timeout: Duration::from_secs(5),
        max_output_bytes: 64 * 1024,
        cancel: CancellationToken::new(),
        sink: Arc::new(MemorySink::new()),
    };
    let out = t.run(serde_json::json!({"command": "echo hello"}), &ctx).await;
    assert!(!out.is_error);
    let txt = out.content.iter().find_map(|b| match b {
        flash_code::protocol::ContentBlock::Text { text } => Some(text.clone()),
        _ => None,
    });
    assert!(txt.unwrap_or_default().contains("hello"));
}

#[tokio::test]
async fn bash_failing_command_marks_error() {
    let t = BashTool;
    let ctx = ExecutionContext {
        session_id: "s1".into(),
        call_id: "c1".into(),
        cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        timeout: Duration::from_secs(5),
        max_output_bytes: 64 * 1024,
        cancel: CancellationToken::new(),
        sink: Arc::new(MemorySink::new()),
    };
    let out = t.run(serde_json::json!({"command": "exit 17"}), &ctx).await;
    assert!(out.is_error);
}

#[tokio::test]
async fn bash_invalid_input_returns_failure() {
    let t = BashTool;
    let ctx = ExecutionContext {
        session_id: "s1".into(),
        call_id: "c1".into(),
        cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        timeout: Duration::from_secs(5),
        max_output_bytes: 64 * 1024,
        cancel: CancellationToken::new(),
        sink: Arc::new(MemorySink::new()),
    };
    // missing required `command` field
    let out = t.run(serde_json::json!({}), &ctx).await;
    assert!(out.is_error);
}
