use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use flash_core::{
    Tool, ToolContext, ToolError, ToolExitStatus, ToolOutput, ToolRegistry, ToolRisk,
};

pub fn builtin_registry() -> Result<ToolRegistry, flash_core::tools::ToolRegistryError> {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(SearchTool))?;
    registry.register(Box::new(ReadFileTool))?;
    registry.register(Box::new(ShellTool::default()))?;
    Ok(registry)
}

pub struct SearchTool;

impl Tool for SearchTool {
    fn name(&self) -> &str {
        "search"
    }

    fn risk(&self, _input: &str) -> ToolRisk {
        ToolRisk::Read
    }

    fn call(&self, input: &str, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        let query = input.trim();
        let mut matches = Vec::new();
        visit_files(&context.workspace_root, &mut |path| {
            if matches.len() >= 200 {
                return;
            }
            let relative = path.strip_prefix(&context.workspace_root).unwrap_or(path);
            let relative_text = relative.display().to_string();
            if query.is_empty()
                || query == "."
                || relative_text.contains(query)
                || path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains(query))
            {
                matches.push(relative_text);
            }
        })?;
        Ok(ToolOutput::success(matches.join("\n")))
    }
}

pub struct ReadFileTool;

impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn risk(&self, _input: &str) -> ToolRisk {
        ToolRisk::Read
    }

    fn call(&self, input: &str, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        let path = workspace_path(&context.workspace_root, input)?;
        let content = fs::read_to_string(path)
            .map_err(|error| ToolError::new(format!("failed to read file: {error}")))?;
        Ok(ToolOutput::success(content))
    }
}

pub struct ShellTool {
    timeout: Duration,
}

impl Default for ShellTool {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
        }
    }
}

impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn risk(&self, input: &str) -> ToolRisk {
        let command = input.trim();
        if command.contains("rm -rf")
            || command.starts_with("rm ")
            || command.contains(" shutdown")
            || command.contains(" mkfs")
        {
            ToolRisk::Destructive
        } else {
            ToolRisk::Execute
        }
    }

    fn call(&self, input: &str, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(input)
            .current_dir(&context.workspace_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| ToolError::new(format!("failed to spawn shell: {error}")))?;

        let started = Instant::now();
        loop {
            if child
                .try_wait()
                .map_err(|error| ToolError::new(format!("failed to poll shell: {error}")))?
                .is_some()
            {
                let output = child
                    .wait_with_output()
                    .map_err(|error| ToolError::new(format!("failed to collect shell: {error}")))?;
                let status = if output.status.success() {
                    ToolExitStatus::Success
                } else {
                    ToolExitStatus::Error
                };
                return Ok(ToolOutput {
                    stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                    stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                    status,
                });
            }
            if started.elapsed() >= self.timeout {
                child
                    .kill()
                    .map_err(|error| ToolError::new(format!("failed to kill shell: {error}")))?;
                return Ok(ToolOutput {
                    stdout: String::new(),
                    stderr: "shell command timed out".to_string(),
                    status: ToolExitStatus::Error,
                });
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn workspace_path(root: &Path, input: &str) -> Result<PathBuf, ToolError> {
    let path = root.join(input.trim());
    let canonical = path
        .canonicalize()
        .map_err(|error| ToolError::new(format!("invalid path: {error}")))?;
    if !canonical.starts_with(root) {
        return Err(ToolError::new("path escapes workspace"));
    }
    Ok(canonical)
}

fn visit_files(root: &Path, visit: &mut impl FnMut(&Path)) -> Result<(), ToolError> {
    for entry in fs::read_dir(root)
        .map_err(|error| ToolError::new(format!("failed to read dir: {error}")))?
    {
        let entry =
            entry.map_err(|error| ToolError::new(format!("failed to read dir: {error}")))?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name == ".git" || name == "target" || name == ".flash" {
            continue;
        }
        if path.is_dir() {
            visit_files(&path, visit)?;
        } else {
            visit(&path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn search_should_find_workspace_files() {
        let root = temp_dir("search");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn ok() {}").unwrap();
        let tool = SearchTool;

        let output = tool
            .call(
                "lib",
                &ToolContext {
                    workspace_root: root,
                },
            )
            .unwrap();

        assert!(output.stdout.contains("src/lib.rs"));
    }

    #[test]
    fn read_file_should_reject_workspace_escape() {
        let root = temp_dir("read_escape");
        fs::create_dir_all(&root).unwrap();
        let outside = temp_dir("outside");
        fs::write(&outside, "secret").unwrap();
        let tool = ReadFileTool;

        let error = tool
            .call(
                outside.to_str().unwrap(),
                &ToolContext {
                    workspace_root: root,
                },
            )
            .unwrap_err();

        assert_eq!(error.message, "path escapes workspace");
    }

    #[test]
    fn shell_should_return_error_on_timeout() {
        let root = temp_dir("shell_timeout");
        fs::create_dir_all(&root).unwrap();
        let tool = ShellTool {
            timeout: Duration::from_millis(1),
        };

        let output = tool
            .call(
                "sleep 1",
                &ToolContext {
                    workspace_root: root,
                },
            )
            .unwrap();

        assert_eq!(output.status, ToolExitStatus::Error);
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("flash_tools_{name}_{nanos}"))
    }
}
