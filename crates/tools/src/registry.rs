use std::time::Duration;

use flash_core::tools::ToolRegistryError;
use flash_core::ToolRegistry;

use crate::exec::{macos_sandbox_status, BashTool};
use crate::fs::{EditTool, GlobTool, GrepTool, ListFilesTool, ReadTool, WriteTool};

pub fn builtin_registry() -> Result<ToolRegistry, ToolRegistryError> {
    builtin_registry_with_options(120, 200_000, false)
}

pub fn builtin_registry_with_options(
    shell_timeout_secs: u64,
    shell_max_output_bytes: usize,
    allow_network: bool,
) -> Result<ToolRegistry, ToolRegistryError> {
    if !allow_network {
        let sandbox = macos_sandbox_status();
        if !sandbox.available {
            return Err(ToolRegistryError::RuntimeUnavailable(format!(
                "network-disabled Bash requires macOS sandbox-exec: {}",
                sandbox.detail
            )));
        }
    }
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(ReadTool))?;
    registry.register(Box::new(EditTool))?;
    registry.register(Box::new(WriteTool))?;
    registry.register(Box::new(GlobTool))?;
    registry.register(Box::new(GrepTool))?;
    registry.register(Box::new(ListFilesTool))?;
    registry.register(Box::new(BashTool {
        timeout: Duration::from_secs(shell_timeout_secs),
        max_output_bytes: shell_max_output_bytes,
        allow_network,
    }))?;
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flash_core::ToolRisk;
    use serde_json::json;

    #[test]
    fn builtin_registry_should_expose_only_canonical_protocol_names() {
        let registry = builtin_registry().unwrap();
        let names = registry.names().collect::<Vec<_>>();

        assert_eq!(
            names,
            vec!["Bash", "Edit", "Glob", "Grep", "ListFiles", "Read", "Write"]
        );
    }

    #[test]
    fn builtin_registry_should_expose_json_schema_objects() {
        let registry = builtin_registry().unwrap();

        for descriptor in registry.descriptors() {
            assert!(descriptor.parameters.is_object(), "{}", descriptor.name);
        }
    }

    #[test]
    fn v1_tool_risks_should_match_protocol() {
        let registry = builtin_registry().unwrap();
        let cases = [
            ("Read", json!({"path": "src/lib.rs"}), ToolRisk::Read),
            (
                "Edit",
                json!({"path": "src/lib.rs", "find": "a", "replace": "b"}),
                ToolRisk::Write,
            ),
            (
                "Write",
                json!({"path": "src/lib.rs", "content": ""}),
                ToolRisk::Write,
            ),
            ("Glob", json!({"pattern": "**/*.rs"}), ToolRisk::Read),
            ("Grep", json!({"pattern": "answer"}), ToolRisk::Read),
            ("ListFiles", json!({"path": "."}), ToolRisk::Read),
            ("Bash", json!({"command": "cargo test"}), ToolRisk::Read),
            (
                "Bash",
                json!({"command": "rm -rf target"}),
                ToolRisk::Destructive,
            ),
        ];

        for (name, input, expected) in cases {
            let tool = registry.get(name).unwrap();
            assert_eq!(
                tool.risk(&input).unwrap(),
                expected,
                "wrong risk for {name}"
            );
        }
    }
}
