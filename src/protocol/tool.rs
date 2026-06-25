use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// Read-only, no side effects.
    Safe,
    /// Modifies local state, recoverable.
    Moderate,
    /// Irreversible / system-affecting.
    Dangerous,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub risk: RiskLevel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Tool explicitly says "this call is safe; auto-approve".
    AutoApprove,
    /// Tool explicitly says "must ask user, even in Yolo mode".
    MustAsk,
    /// Tool defers to ApprovalGate's default decision.
    Default,
}

#[derive(Debug, Clone)]
pub struct ToolApprovalAdvice {
    pub decision: ApprovalDecision,
    pub reason: Option<String>,
}

impl ToolApprovalAdvice {
    #[must_use]
    pub fn default_for(_risk: RiskLevel) -> Self {
        Self {
            decision: ApprovalDecision::Default,
            reason: None,
        }
    }

    #[must_use]
    pub fn auto_approve(reason: impl Into<String>) -> Self {
        Self {
            decision: ApprovalDecision::AutoApprove,
            reason: Some(reason.into()),
        }
    }

    #[must_use]
    pub fn must_ask(reason: impl Into<String>) -> Self {
        Self {
            decision: ApprovalDecision::MustAsk,
            reason: Some(reason.into()),
        }
    }
}
