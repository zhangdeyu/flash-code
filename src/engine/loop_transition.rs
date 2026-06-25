#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopTransition {
    Initial,
    ToolResultReturn,
    OverflowRetried,
    MaxTokensRetried,
    TransientRetried,
}

impl LoopTransition {
    #[must_use]
    pub fn is_post_retry(self) -> bool {
        matches!(
            self,
            Self::OverflowRetried | Self::MaxTokensRetried | Self::TransientRetried
        )
    }
}
