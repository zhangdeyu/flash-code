use std::fs;

use flash_core::{Tool, ToolContext, ToolError, ToolErrorKind, ToolOutput, ToolRisk};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::parse_input;
use crate::path_guard::workspace_path;

pub struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read the full or partial content of a file within the workspace."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Relative path to the file to read"},
                "start_line": {"type": "integer", "minimum": 1, "description": "First line to read (1-indexed, inclusive)"},
                "end_line": {"type": "integer", "minimum": 1, "description": "Last line to read (1-indexed, inclusive)"}
            },
            "required": ["path"]
        })
    }

    fn risk(&self, _input: &Value) -> Result<ToolRisk, ToolError> {
        Ok(ToolRisk::Read)
    }

    fn call(&self, input: Value, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        let input: ReadInput = parse_input(input)?;
        read_workspace_file(&input, context)
    }
}

#[derive(Debug, Deserialize)]
struct ReadInput {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

fn read_workspace_file(input: &ReadInput, context: &ToolContext) -> Result<ToolOutput, ToolError> {
    let path = workspace_path(&context.workspace_root, &input.path)?;
    let content = fs::read_to_string(path).map_err(|error| {
        ToolError::with_kind(ToolErrorKind::Io, format!("failed to read file: {error}"))
    })?;
    let Some(start_line) = input.start_line else {
        return Ok(ToolOutput::success(content));
    };
    if start_line == 0 {
        return Err(ToolError::with_kind(
            ToolErrorKind::InvalidInput,
            "start_line must be greater than 0",
        ));
    }
    let end_line = input.end_line.unwrap_or(usize::MAX);
    if end_line < start_line {
        return Err(ToolError::with_kind(
            ToolErrorKind::InvalidInput,
            "end_line must be greater than or equal to start_line",
        ));
    }
    let selected = content
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let line_number = index + 1;
            (line_number >= start_line && line_number <= end_line).then_some(line)
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(ToolOutput::success(selected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{context, temp_dir};

    #[test]
    fn read_should_read_file_contents() {
        let root = temp_dir("read_success");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn ok() {}").unwrap();
        let tool = ReadTool;

        let output = tool
            .call(json!({"path": "src/lib.rs"}), &context(root))
            .unwrap();

        assert_eq!(output.stdout, "pub fn ok() {}");
    }

    #[test]
    fn read_should_read_line_range() {
        let root = temp_dir("read_range");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "one\ntwo\nthree\n").unwrap();
        let tool = ReadTool;

        let output = tool
            .call(
                json!({"path": "src/lib.rs", "start_line": 2, "end_line": 3}),
                &context(root),
            )
            .unwrap();

        assert_eq!(output.stdout, "two\nthree");
    }

    #[test]
    fn read_should_report_missing_file() {
        let root = temp_dir("read_missing");
        fs::create_dir_all(&root).unwrap();
        let tool = ReadTool;

        let error = tool
            .call(json!({"path": "missing.rs"}), &context(root))
            .unwrap_err();

        assert!(error.message.contains("invalid path"));
    }

    #[test]
    fn read_should_reject_workspace_escape() {
        let root = temp_dir("read_escape");
        fs::create_dir_all(&root).unwrap();
        let outside = temp_dir("outside");
        fs::write(&outside, "secret").unwrap();
        let tool = ReadTool;

        let error = tool
            .call(json!({"path": outside.to_str().unwrap()}), &context(root))
            .unwrap_err();

        assert_eq!(error.message, "path escapes workspace");
        assert_eq!(error.kind, ToolErrorKind::PermissionDenied);
    }
}
