use std::fs;

use flash_core::{Tool, ToolContext, ToolError, ToolOutput, ToolRisk};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::parse_input;
use crate::path_guard::writable_workspace_path;

pub struct WriteTool;

impl Tool for WriteTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn description(&self) -> &str {
        "Create a new file or completely overwrite an existing file with the given content."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Relative path to the file to write"},
                "content": {"type": "string", "description": "Full content to write to the file"}
            },
            "required": ["path", "content"]
        })
    }

    fn risk(&self, _input: &Value) -> Result<ToolRisk, ToolError> {
        Ok(ToolRisk::Write)
    }

    fn call(&self, input: Value, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        let input: WriteInput = parse_input(input)?;
        write_workspace_file(&input, context)
    }
}

#[derive(Debug, Deserialize)]
struct WriteInput {
    path: String,
    content: String,
}

fn write_workspace_file(
    input: &WriteInput,
    context: &ToolContext,
) -> Result<ToolOutput, ToolError> {
    let path = writable_workspace_path(&context.workspace_root, &input.path)?;
    fs::write(&path, &input.content)
        .map_err(|error| ToolError::new(format!("failed to write file: {error}")))?;
    let root = context
        .workspace_root
        .canonicalize()
        .map_err(|error| ToolError::new(format!("invalid workspace root: {error}")))?;
    Ok(ToolOutput::success(format!(
        "wrote {}",
        path.strip_prefix(&root).unwrap_or(&path).display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{context, temp_dir};

    #[test]
    fn write_should_create_file_contents() {
        let root = temp_dir("write_success");
        fs::create_dir_all(root.join("src")).unwrap();
        let tool = WriteTool;

        let output = tool
            .call(
                json!({"path": "src/lib.rs", "content": "pub fn ok() {}"}),
                &context(root.clone()),
            )
            .unwrap();

        assert!(output.stdout.contains("wrote src/lib.rs"));
        assert_eq!(
            fs::read_to_string(root.join("src/lib.rs")).unwrap(),
            "pub fn ok() {}"
        );
    }

    #[test]
    fn write_should_reject_parent_dir_escape() {
        let root = temp_dir("write_escape_parent");
        fs::create_dir_all(&root).unwrap();
        let tool = WriteTool;

        let error = tool
            .call(
                json!({"path": "../outside.txt", "content": "nope"}),
                &context(root),
            )
            .unwrap_err();

        assert_eq!(error.message, "path escapes workspace");
    }

    #[test]
    fn write_should_reject_symlink_escape() {
        let root = temp_dir("write_escape_symlink");
        let outside = temp_dir("outside_dir");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let tool = WriteTool;

        let error = tool
            .call(
                json!({"path": "link/outside.txt", "content": "nope"}),
                &context(root),
            )
            .unwrap_err();

        assert_eq!(error.message, "path escapes workspace");
    }
}
