use std::fs;

use flash_core::{Tool, ToolContext, ToolError, ToolErrorKind, ToolOutput, ToolRisk};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::parse_input;
use crate::path_guard::existing_workspace_path;

pub struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> &str {
        "Make a targeted find-and-replace edit to an existing file. The find text must match exactly."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Relative path to the file to edit"},
                "find": {"type": "string", "description": "Exact text to find in the file"},
                "replace": {"type": "string", "description": "Replacement text"}
            },
            "required": ["path", "find", "replace"]
        })
    }

    fn risk(&self, _input: &Value) -> Result<ToolRisk, ToolError> {
        Ok(ToolRisk::Write)
    }

    fn call(&self, input: Value, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        let input: EditInput = parse_input(input)?;
        apply_replace_patch(&input, context)
    }
}

#[derive(Debug, Deserialize)]
struct EditInput {
    path: String,
    find: String,
    replace: String,
}

fn apply_replace_patch(input: &EditInput, context: &ToolContext) -> Result<ToolOutput, ToolError> {
    let path = existing_workspace_path(&context.workspace_root, &input.path)?;
    let content = fs::read_to_string(&path).map_err(|error| {
        ToolError::with_kind(
            ToolErrorKind::Io,
            format!("failed to read patch target: {error}"),
        )
    })?;
    if !content.contains(&input.find) {
        return Err(ToolError::with_kind(
            ToolErrorKind::InvalidInput,
            "edit failed: find text not found",
        ));
    }
    let updated = content.replacen(&input.find, &input.replace, 1);
    fs::write(&path, updated)
        .map_err(|error| ToolError::new(format!("failed to write patch target: {error}")))?;
    let root = context
        .workspace_root
        .canonicalize()
        .map_err(|error| ToolError::new(format!("invalid workspace root: {error}")))?;
    Ok(ToolOutput::success(format!(
        "patched {}",
        path.strip_prefix(&root).unwrap_or(&path).display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{context, temp_dir};

    #[test]
    fn edit_should_replace_text_inside_workspace() {
        let root = temp_dir("edit");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> i32 { 41 }\n").unwrap();
        let tool = EditTool;

        let output = tool
            .call(
                json!({"path": "src/lib.rs", "find": "41", "replace": "42"}),
                &context(root),
            )
            .unwrap();

        assert!(output.stdout.contains("patched src/lib.rs"));
    }

    #[test]
    fn edit_should_report_find_text_missing() {
        let root = temp_dir("edit_missing_find");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> i32 { 42 }\n").unwrap();
        let tool = EditTool;

        let error = tool
            .call(
                json!({"path": "src/lib.rs", "find": "41", "replace": "42"}),
                &context(root),
            )
            .unwrap_err();

        assert_eq!(error.message, "edit failed: find text not found");
        assert_eq!(error.kind, ToolErrorKind::InvalidInput);
    }

    #[test]
    fn edit_should_reject_parent_dir_escape() {
        let root = temp_dir("edit_escape_parent");
        fs::create_dir_all(&root).unwrap();
        let tool = EditTool;

        let error = tool
            .call(
                json!({"path": "../outside.rs", "find": "old", "replace": "new"}),
                &context(root),
            )
            .unwrap_err();

        assert_eq!(error.message, "path escapes workspace");
    }
}
