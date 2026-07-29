use std::fs;

use flash_core::{Tool, ToolContext, ToolError, ToolOutput, ToolRisk};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::parse_input;

pub struct GrepTool;

impl Tool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &str {
        "Search for a keyword or pattern in file contents across the workspace. Returns file:line:content matches."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Keyword or substring to search for in file contents"}
            },
            "required": ["pattern"]
        })
    }

    fn risk(&self, _input: &Value) -> Result<ToolRisk, ToolError> {
        Ok(ToolRisk::Read)
    }

    fn call(&self, input: Value, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        let input: GrepInput = parse_input(input)?;
        let mut matches = Vec::new();
        super::visit_files(&context.workspace_root, &mut |path| {
            if matches.len() >= 200 {
                return;
            }
            let Ok(content) = fs::read_to_string(path) else {
                return;
            };
            for (line_index, line) in content.lines().enumerate() {
                if line.contains(&input.pattern) {
                    let relative = path.strip_prefix(&context.workspace_root).unwrap_or(path);
                    matches.push(format!(
                        "{}:{}:{}",
                        relative.display(),
                        line_index + 1,
                        line
                    ));
                }
            }
        })?;
        Ok(ToolOutput::success(matches.join("\n")))
    }
}

#[derive(Debug, Deserialize)]
struct GrepInput {
    pattern: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{context, temp_dir};

    #[test]
    fn grep_should_search_file_contents() {
        let root = temp_dir("grep");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> i32 { 42 }").unwrap();
        let tool = GrepTool;

        let output = tool
            .call(json!({"pattern": "answer"}), &context(root))
            .unwrap();

        assert!(output.stdout.contains("src/lib.rs:1"));
    }
}
