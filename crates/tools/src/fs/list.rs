use std::fs;

use flash_core::{Tool, ToolContext, ToolError, ToolOutput, ToolRisk};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::parse_input;
use crate::path_guard::workspace_path;

pub struct ListFilesTool;

impl Tool for ListFilesTool {
    fn name(&self) -> &str {
        "ListFiles"
    }

    fn description(&self) -> &str {
        "List the immediate children (files and directories) of a path in the workspace."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Relative path to the directory to list. Defaults to workspace root if omitted."}
            },
            "required": []
        })
    }

    fn risk(&self, _input: &Value) -> Result<ToolRisk, ToolError> {
        Ok(ToolRisk::Read)
    }

    fn call(&self, input: Value, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        let input: ListFilesInput = parse_input(input)?;
        let path = match input.path.as_deref() {
            None | Some("") | Some(".") => context.workspace_root.clone(),
            Some(path) => workspace_path(&context.workspace_root, path)?,
        };
        let mut entries = Vec::new();
        for entry in fs::read_dir(&path)
            .map_err(|error| ToolError::new(format!("failed to list files: {error}")))?
        {
            let entry =
                entry.map_err(|error| ToolError::new(format!("failed to list files: {error}")))?;
            let entry_path = entry.path();
            let Some(name) = entry_path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name == ".git" || name == "target" || name == ".flash" {
                continue;
            }
            let suffix = if entry_path.is_dir() { "/" } else { "" };
            entries.push(format!("{name}{suffix}"));
        }
        entries.sort();
        Ok(ToolOutput::success(entries.join("\n")))
    }
}

#[derive(Debug, Deserialize)]
struct ListFilesInput {
    path: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{context, temp_dir};

    #[test]
    fn list_files_should_list_directory_entries() {
        let root = temp_dir("list_files");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "").unwrap();
        let tool = ListFilesTool;

        let output = tool.call(json!({"path": "."}), &context(root)).unwrap();

        assert!(output.stdout.contains("src/"));
    }
}
