use serde::{Deserialize, Serialize};

/// Specification of a tool that the model can call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// A tool call requested by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Directly reuse the id returned by the provider.
    pub call_id: String,
    pub name: String,
    pub input: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_spec_clone() {
        let spec = ToolSpec {
            name: "bash".into(),
            description: "Execute a shell command".into(),
            input_schema: serde_json::json!({"type": "object"}),
        };
        let cloned = spec.clone();
        assert_eq!(cloned.name, "bash");
    }

    #[test]
    fn tool_call_serializes() {
        let call = ToolCall {
            call_id: "c1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "ls"}),
        };
        let json = serde_json::to_string(&call).expect("serialize");
        assert!(json.contains("\"call_id\":\"c1\""));
    }
}
// ToolSpec, ToolCall
