use flash_core::{Event, ToolRisk};

pub trait EventObserver {
    fn on_event(&mut self, event: &Event);
}

impl<F> EventObserver for F
where
    F: FnMut(&Event),
{
    fn on_event(&mut self, event: &Event) {
        self(event);
    }
}

pub(crate) struct NoopObserver;

impl EventObserver for NoopObserver {
    fn on_event(&mut self, _event: &Event) {}
}

pub trait ApprovalController {
    fn approve(&mut self, request: &ApprovalRequest) -> bool;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub call_id: String,
    pub name: String,
    pub input: String,
    pub risk: ToolRisk,
}

pub(crate) struct RejectingApproval;

impl ApprovalController for RejectingApproval {
    fn approve(&mut self, _request: &ApprovalRequest) -> bool {
        false
    }
}
