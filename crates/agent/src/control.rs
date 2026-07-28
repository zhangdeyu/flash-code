use flash_core::CancellationToken;

pub(crate) enum SessionStart {
    New,
    Continue(String),
}

pub(crate) struct ExecutionControls<'a, C, A> {
    pub(crate) cancellation: CancellationToken,
    pub(crate) should_cancel: C,
    pub(crate) approval: &'a mut A,
}
