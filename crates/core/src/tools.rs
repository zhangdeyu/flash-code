use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ToolRisk {
    Read,
    Write,
    Execute,
    Network,
    Destructive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalMode {
    Confirm,
    Yolo,
    Human,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    pub message: String,
}

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    fn risk(&self, input: &str) -> ToolRisk;

    fn call(&self, input: &str, context: &ToolContext) -> Result<ToolOutput, ToolError>;
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Box<dyn Tool>>,
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
        self.tools.insert(name, tool);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(Box::as_ref)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolRegistryError {
    DuplicateName(String),
}

impl std::fmt::Display for ToolRegistryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateName(name) => write!(formatter, "tool `{name}` is already registered"),
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

        fn risk(&self, _input: &str) -> ToolRisk {
            ToolRisk::Read
        }

        fn call(&self, _input: &str, _context: &ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                text: "ok".to_string(),
            })
        }
    }

    #[test]
    fn register_should_reject_duplicate_tool_names() {
        let mut registry = ToolRegistry::new();
        registry
            .register(Box::new(FakeTool { name: "read_file" }))
            .unwrap();

        let error = registry
            .register(Box::new(FakeTool { name: "read_file" }))
            .unwrap_err();

        assert_eq!(
            error,
            ToolRegistryError::DuplicateName("read_file".to_string())
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
}
