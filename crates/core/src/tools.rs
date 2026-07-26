use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Notify;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRisk {
    Read,
    Write,
    Execute,
    Network,
    Destructive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    Confirm,
    Yolo,
    Human,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionPolicy {
    mode: ApprovalMode,
}

impl PermissionPolicy {
    pub const fn new(mode: ApprovalMode) -> Self {
        Self { mode }
    }

    pub const fn decide(self, risk: ToolRisk) -> PermissionDecision {
        match (self.mode, risk) {
            (ApprovalMode::Human, _) => PermissionDecision::Deny,
            (_, ToolRisk::Destructive) => PermissionDecision::Ask,
            (ApprovalMode::Confirm, ToolRisk::Read) => PermissionDecision::Allow,
            (ApprovalMode::Confirm, _) => PermissionDecision::Ask,
            (ApprovalMode::Yolo, ToolRisk::Read | ToolRisk::Write | ToolRisk::Execute) => {
                PermissionDecision::Allow
            }
            (ApprovalMode::Yolo, ToolRisk::Network) => PermissionDecision::Ask,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolContext {
    pub workspace_root: PathBuf,
    pub cancellation: CancellationToken,
    pub artifact_dir: Option<PathBuf>,
    pub artifact_stem: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::SeqCst) {
            self.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let notified = self.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

impl PartialEq for CancellationToken {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.cancelled, &other.cancelled)
    }
}

impl Eq for CancellationToken {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub stdout: String,
    pub stderr: String,
    pub status: ToolExitStatus,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub truncated: bool,
    pub artifact: Option<String>,
}

impl ToolOutput {
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: String::new(),
            status: ToolExitStatus::Success,
            exit_code: Some(0),
            signal: None,
            duration_ms: 0,
            timed_out: false,
            truncated: false,
            artifact: None,
        }
    }

    pub fn error(stderr: impl Into<String>) -> Self {
        Self {
            stdout: String::new(),
            stderr: stderr.into(),
            status: ToolExitStatus::Error,
            exit_code: None,
            signal: None,
            duration_ms: 0,
            timed_out: false,
            truncated: false,
            artifact: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExitStatus {
    Success,
    Error,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    pub kind: ToolErrorKind,
    pub message: String,
}

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            kind: ToolErrorKind::Internal,
            message: message.into(),
        }
    }

    pub fn with_kind(kind: ToolErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorKind {
    InvalidInput,
    PermissionDenied,
    Io,
    Timeout,
    Cancelled,
    ProcessFailed,
    Internal,
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    /// One-line description of what the tool does.
    fn description(&self) -> &str;

    /// JSON Schema object for the tool's input parameters.
    fn parameters(&self) -> Value;

    fn risk(&self, input: &Value) -> Result<ToolRisk, ToolError>;

    fn call(&self, input: Value, context: &ToolContext) -> Result<ToolOutput, ToolError>;
}

/// Descriptor combining a tool's name, description and JSON Schema parameters.
/// Used to build provider-specific tool specs (e.g. DeepSeek function-calling).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) -> Result<(), ToolRegistryError> {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            return Err(ToolRegistryError::DuplicateName(name));
        }
        let parameters = tool.parameters();
        if let Err(message) = validate_parameters_schema(&parameters) {
            return Err(ToolRegistryError::InvalidSchema { name, message });
        }
        self.tools.insert(name, Arc::from(tool));
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(Arc::as_ref)
    }

    pub async fn call_blocking(
        &self,
        name: &str,
        input: Value,
        context: ToolContext,
    ) -> Option<Result<ToolOutput, ToolError>> {
        let tool = Arc::clone(self.tools.get(name)?);
        Some(
            tokio::task::spawn_blocking(move || tool.call(input, &context))
                .await
                .unwrap_or_else(|error| Err(ToolError::new(format!("tool task failed: {error}")))),
        )
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }

    /// Returns a `ToolDescriptor` (name + description + parameters) for every registered tool.
    pub fn descriptors(&self) -> impl Iterator<Item = ToolDescriptor> + '_ {
        self.tools.values().map(|tool| ToolDescriptor {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            parameters: tool.parameters(),
        })
    }
}

fn validate_parameters_schema(schema: &Value) -> Result<(), String> {
    let object = schema
        .as_object()
        .ok_or_else(|| "tool parameters must be a JSON object".to_string())?;
    if object.get("type").and_then(Value::as_str) != Some("object") {
        return Err("tool parameters root type must be `object`".to_string());
    }
    let properties = object
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| "tool parameters must define an object `properties` map".to_string())?;
    if let Some(required) = object.get("required") {
        let required = required
            .as_array()
            .ok_or_else(|| "tool parameters `required` must be an array".to_string())?;
        for field in required {
            let field = field
                .as_str()
                .ok_or_else(|| "tool parameters `required` entries must be strings".to_string())?;
            if !properties.contains_key(field) {
                return Err(format!(
                    "required tool parameter `{field}` is missing from `properties`"
                ));
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolRegistryError {
    DuplicateName(String),
    InvalidSchema { name: String, message: String },
}

impl std::fmt::Display for ToolRegistryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateName(name) => write!(formatter, "tool `{name}` is already registered"),
            Self::InvalidSchema { name, message } => {
                write!(formatter, "tool `{name}` has invalid schema: {message}")
            }
        }
    }
}

impl std::error::Error for ToolRegistryError {}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeTool {
        name: &'static str,
    }

    impl Tool for FakeTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "fake tool for testing"
        }

        fn parameters(&self) -> Value {
            serde_json::json!({"type":"object","properties":{}})
        }

        fn risk(&self, _input: &Value) -> Result<ToolRisk, ToolError> {
            Ok(ToolRisk::Read)
        }

        fn call(&self, _input: Value, _context: &ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::success("ok"))
        }
    }

    #[test]
    fn register_should_reject_duplicate_tool_names() {
        let mut registry = ToolRegistry::new();
        registry
            .register(Box::new(FakeTool { name: "fake" }))
            .unwrap();

        let error = registry
            .register(Box::new(FakeTool { name: "fake" }))
            .unwrap_err();

        assert_eq!(error, ToolRegistryError::DuplicateName("fake".to_string()));
    }

    struct InvalidSchemaTool;

    impl Tool for InvalidSchemaTool {
        fn name(&self) -> &str {
            "bad"
        }

        fn description(&self) -> &str {
            "bad schema"
        }

        fn parameters(&self) -> Value {
            Value::String("not an object".to_string())
        }

        fn risk(&self, _input: &Value) -> Result<ToolRisk, ToolError> {
            Ok(ToolRisk::Read)
        }

        fn call(&self, _input: Value, _context: &ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::success("ok"))
        }
    }

    #[test]
    fn register_should_reject_non_object_schema() {
        let mut registry = ToolRegistry::new();

        let error = registry.register(Box::new(InvalidSchemaTool)).unwrap_err();

        assert_eq!(
            error,
            ToolRegistryError::InvalidSchema {
                name: "bad".to_string(),
                message: "tool parameters must be a JSON object".to_string(),
            }
        );
    }

    #[test]
    fn schema_validation_should_reject_invalid_root_and_required_fields() {
        assert_eq!(
            validate_parameters_schema(&serde_json::json!({
                "type": "string",
                "properties": {}
            })),
            Err("tool parameters root type must be `object`".to_string())
        );
        assert_eq!(
            validate_parameters_schema(&serde_json::json!({
                "type": "object",
                "properties": {},
                "required": ["missing"]
            })),
            Err("required tool parameter `missing` is missing from `properties`".to_string())
        );
    }

    #[test]
    fn permission_policy_should_require_confirmation_for_destructive_yolo() {
        let policy = PermissionPolicy::new(ApprovalMode::Yolo);

        assert_eq!(
            policy.decide(ToolRisk::Destructive),
            PermissionDecision::Ask
        );
    }

    #[test]
    fn permission_policy_should_deny_execution_in_human_mode() {
        let policy = PermissionPolicy::new(ApprovalMode::Human);

        assert_eq!(policy.decide(ToolRisk::Execute), PermissionDecision::Deny);
    }

    #[test]
    fn permission_policy_should_cover_confirm_yolo_and_human_modes() {
        let cases = [
            (
                ApprovalMode::Confirm,
                ToolRisk::Read,
                PermissionDecision::Allow,
            ),
            (
                ApprovalMode::Confirm,
                ToolRisk::Write,
                PermissionDecision::Ask,
            ),
            (
                ApprovalMode::Confirm,
                ToolRisk::Execute,
                PermissionDecision::Ask,
            ),
            (
                ApprovalMode::Confirm,
                ToolRisk::Network,
                PermissionDecision::Ask,
            ),
            (
                ApprovalMode::Confirm,
                ToolRisk::Destructive,
                PermissionDecision::Ask,
            ),
            (
                ApprovalMode::Yolo,
                ToolRisk::Read,
                PermissionDecision::Allow,
            ),
            (
                ApprovalMode::Yolo,
                ToolRisk::Write,
                PermissionDecision::Allow,
            ),
            (
                ApprovalMode::Yolo,
                ToolRisk::Execute,
                PermissionDecision::Allow,
            ),
            (
                ApprovalMode::Yolo,
                ToolRisk::Network,
                PermissionDecision::Ask,
            ),
            (
                ApprovalMode::Yolo,
                ToolRisk::Destructive,
                PermissionDecision::Ask,
            ),
            (
                ApprovalMode::Human,
                ToolRisk::Read,
                PermissionDecision::Deny,
            ),
            (
                ApprovalMode::Human,
                ToolRisk::Write,
                PermissionDecision::Deny,
            ),
            (
                ApprovalMode::Human,
                ToolRisk::Execute,
                PermissionDecision::Deny,
            ),
            (
                ApprovalMode::Human,
                ToolRisk::Network,
                PermissionDecision::Deny,
            ),
            (
                ApprovalMode::Human,
                ToolRisk::Destructive,
                PermissionDecision::Deny,
            ),
        ];

        for (mode, risk, expected) in cases {
            assert_eq!(PermissionPolicy::new(mode).decide(risk), expected);
        }
    }

    #[test]
    fn permission_enums_should_use_stable_snake_case_json_names() {
        assert_eq!(
            serde_json::to_string(&ToolRisk::Destructive).unwrap(),
            r#""destructive""#
        );
        assert_eq!(
            serde_json::from_str::<ApprovalMode>(r#""yolo""#).unwrap(),
            ApprovalMode::Yolo
        );
        assert_eq!(
            serde_json::from_str::<PermissionDecision>(r#""ask""#).unwrap(),
            PermissionDecision::Ask
        );
    }
}
