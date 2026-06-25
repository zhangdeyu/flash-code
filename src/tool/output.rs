use crate::protocol::ContentBlock;

/// Unified tool output. Returned by `Tool::run`, then converted to ContentBlock::ToolResult.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
    pub title: Option<String>,
    pub metadata: serde_json::Value,
}

impl ToolOutput {
    fn validate_content(blocks: &[ContentBlock]) {
        for b in blocks {
            assert!(
                b.is_text_or_image(),
                "ToolOutput.content only allows Text/Image blocks"
            );
        }
    }

    #[must_use]
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(s)],
            is_error: false,
            title: None,
            metadata: serde_json::Value::Null,
        }
    }

    #[must_use]
    pub fn blocks(blocks: Vec<ContentBlock>) -> Self {
        Self::validate_content(&blocks);
        Self {
            content: blocks,
            is_error: false,
            title: None,
            metadata: serde_json::Value::Null,
        }
    }

    #[must_use]
    pub fn failure(msg: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(msg)],
            is_error: true,
            title: None,
            metadata: serde_json::Value::Null,
        }
    }

    pub(crate) fn failure_invalid_input(err: serde_json::Error) -> Self {
        Self::failure(format!("invalid input: {err}"))
    }

    #[allow(dead_code)]
    pub(crate) fn cancelled() -> Self {
        Self {
            content: vec![ContentBlock::text("cancelled")],
            is_error: true,
            title: None,
            metadata: serde_json::Value::Null,
        }
    }

    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    #[must_use]
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }
}
