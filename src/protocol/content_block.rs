use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        call_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        call_id: String,
        content: Vec<ContentBlock>,
        is_error: bool,
    },
    Image {
        source: ImageSource,
    },
    Reasoning {
        text: String,
        signature: Option<String>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ContentError {
    #[error("illegal nested block in tool_result content: only Text/Image allowed")]
    IllegalNestedBlock,
}

impl ContentBlock {
    #[must_use]
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }

    /// Construct a ToolResult, validating nested content (Text / Image only).
    pub fn tool_result(
        call_id: impl Into<String>,
        content: Vec<ContentBlock>,
        is_error: bool,
    ) -> Result<Self, ContentError> {
        for b in &content {
            match b {
                ContentBlock::Text { .. } | ContentBlock::Image { .. } => {}
                _ => return Err(ContentError::IllegalNestedBlock),
            }
        }
        Ok(Self::ToolResult {
            call_id: call_id.into(),
            content,
            is_error,
        })
    }

    #[must_use]
    pub fn is_text_or_image(&self) -> bool {
        matches!(self, ContentBlock::Text { .. } | ContentBlock::Image { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_result_accepts_text_and_image() {
        let blocks = vec![
            ContentBlock::text("ok"),
            ContentBlock::Image {
                source: ImageSource::Url {
                    url: "https://example.com/x.png".into(),
                },
            },
        ];
        let r = ContentBlock::tool_result("c1", blocks, false);
        assert!(r.is_ok());
    }

    #[test]
    fn tool_result_rejects_nested_tool_use() {
        let blocks = vec![ContentBlock::ToolUse {
            call_id: "c2".into(),
            name: "x".into(),
            input: serde_json::json!({}),
        }];
        let r = ContentBlock::tool_result("c1", blocks, false);
        assert!(matches!(r, Err(ContentError::IllegalNestedBlock)));
    }

    #[test]
    fn tool_result_rejects_nested_reasoning() {
        let blocks = vec![ContentBlock::Reasoning {
            text: "thinking".into(),
            signature: None,
        }];
        let r = ContentBlock::tool_result("c1", blocks, false);
        assert!(matches!(r, Err(ContentError::IllegalNestedBlock)));
    }
}
