use crate::protocol::{ApprovalDecision, RiskLevel, ToolApprovalAdvice};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalMode {
    Yolo,
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    AutoApprove,
    Ask,
}

pub struct ApprovalGate {
    pub mode: ApprovalMode,
}

impl ApprovalGate {
    #[must_use]
    pub fn decide(&self, risk: RiskLevel, advice: ToolApprovalAdvice) -> ApprovalOutcome {
        // MustAsk always wins, even in Yolo mode.
        if matches!(advice.decision, ApprovalDecision::MustAsk) {
            return ApprovalOutcome::Ask;
        }

        match self.mode {
            ApprovalMode::Yolo => ApprovalOutcome::AutoApprove,
            ApprovalMode::Default => match (risk, advice.decision) {
                (_, ApprovalDecision::AutoApprove) => ApprovalOutcome::AutoApprove,
                (RiskLevel::Safe, ApprovalDecision::Default) => ApprovalOutcome::AutoApprove,
                _ => ApprovalOutcome::Ask,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yolo_must_ask_overrides() {
        let g = ApprovalGate {
            mode: ApprovalMode::Yolo,
        };
        let advice = ToolApprovalAdvice::must_ask("destructive");
        assert_eq!(g.decide(RiskLevel::Safe, advice), ApprovalOutcome::Ask);
    }

    #[test]
    fn yolo_otherwise_auto() {
        let g = ApprovalGate {
            mode: ApprovalMode::Yolo,
        };
        let advice = ToolApprovalAdvice::default_for(RiskLevel::Dangerous);
        assert_eq!(
            g.decide(RiskLevel::Dangerous, advice),
            ApprovalOutcome::AutoApprove
        );
    }

    #[test]
    fn default_safe_default_auto() {
        let g = ApprovalGate {
            mode: ApprovalMode::Default,
        };
        let advice = ToolApprovalAdvice::default_for(RiskLevel::Safe);
        assert_eq!(
            g.decide(RiskLevel::Safe, advice),
            ApprovalOutcome::AutoApprove
        );
    }

    #[test]
    fn default_dangerous_asks() {
        let g = ApprovalGate {
            mode: ApprovalMode::Default,
        };
        let advice = ToolApprovalAdvice::default_for(RiskLevel::Dangerous);
        assert_eq!(g.decide(RiskLevel::Dangerous, advice), ApprovalOutcome::Ask);
    }

    #[test]
    fn default_auto_advice_overrides_risk() {
        let g = ApprovalGate {
            mode: ApprovalMode::Default,
        };
        let advice = ToolApprovalAdvice::auto_approve("read-only");
        assert_eq!(
            g.decide(RiskLevel::Dangerous, advice),
            ApprovalOutcome::AutoApprove
        );
    }
}
