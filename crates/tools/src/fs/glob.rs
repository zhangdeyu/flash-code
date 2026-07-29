use flash_core::{Tool, ToolContext, ToolError, ToolOutput, ToolRisk};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::parse_input;

pub struct GlobTool;

impl Tool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &str {
        "Find files and directories matching a glob pattern (e.g. `**/*.rs`, `src/*.toml`)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern to match against relative file paths"}
            },
            "required": ["pattern"]
        })
    }

    fn risk(&self, _input: &Value) -> Result<ToolRisk, ToolError> {
        Ok(ToolRisk::Read)
    }

    fn call(&self, input: Value, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        let input: GlobInput = parse_input(input)?;
        let mut matches = Vec::new();
        super::visit_files(&context.workspace_root, &mut |path| {
            if matches.len() >= 200 {
                return;
            }
            let relative = path.strip_prefix(&context.workspace_root).unwrap_or(path);
            let relative_text = relative.display().to_string();
            if glob_match(&input.pattern, &relative_text) {
                matches.push(relative_text);
            }
        })?;
        Ok(ToolOutput::success(matches.join("\n")))
    }
}

#[derive(Debug, Deserialize)]
struct GlobInput {
    pattern: String,
}

fn glob_match(pattern: &str, path: &str) -> bool {
    if pattern.is_empty() || pattern == "*" || pattern == "**/*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix("**/*") {
        return path.ends_with(suffix);
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return path.starts_with(prefix);
    }
    if pattern.contains('*') {
        let parts = pattern.split('*').filter(|part| !part.is_empty());
        let mut remainder = path;
        for part in parts {
            let Some(index) = remainder.find(part) else {
                return false;
            };
            remainder = &remainder[index + part.len()..];
        }
        true
    } else {
        path == pattern || path.contains(pattern)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{context, temp_dir};
    use std::fs;

    #[test]
    fn glob_should_match_file_patterns() {
        let root = temp_dir("glob");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        let tool = GlobTool;

        let output = tool
            .call(json!({"pattern": "**/*.rs"}), &context(root))
            .unwrap();

        assert!(output.stdout.contains("src/lib.rs"));
    }
}
