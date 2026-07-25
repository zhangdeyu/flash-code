use std::fs;
use std::path::Component;
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
    registry.register(Box::new(ApplyPatchTool))?;
    registry.register(Box::new(WriteFileTool))?;
    registry.register(Box::new(GitDiffTool))?;
    registry.register(Box::new(RunTestsTool::default()))?;
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

pub struct ApplyPatchTool;

impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn risk(&self, _input: &str) -> ToolRisk {
        ToolRisk::Write
    }

    fn call(&self, input: &str, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        let patch = ReplacePatch::parse(input)?;
        let path = existing_workspace_path(&context.workspace_root, patch.path)?;
        let content = fs::read_to_string(&path)
            .map_err(|error| ToolError::new(format!("failed to read patch target: {error}")))?;
        if !content.contains(patch.find) {
            return Err(ToolError::new("apply_patch failed: find text not found"));
        }
        let updated = content.replacen(patch.find, patch.replace, 1);
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
}

pub struct WriteFileTool;

impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn risk(&self, _input: &str) -> ToolRisk {
        ToolRisk::Write
    }

    fn call(&self, input: &str, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Some((path_text, content)) = input.split_once("\n---CONTENT---\n") else {
            return Err(ToolError::new(
                "write_file input must be `<path>\\n---CONTENT---\\n<content>`",
            ));
        };
        let path = writable_workspace_path(&context.workspace_root, path_text)?;
        fs::write(&path, content)
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
}

pub struct GitDiffTool;

impl Tool for GitDiffTool {
    fn name(&self) -> &str {
        "git_diff"
    }

    fn risk(&self, _input: &str) -> ToolRisk {
        ToolRisk::Read
    }

    fn call(&self, _input: &str, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        command_output("git", &["diff", "--"], &context.workspace_root, None)
    }
}

pub struct RunTestsTool {
    timeout: Duration,
}

impl Default for RunTestsTool {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
        }
    }
}

impl Tool for RunTestsTool {
    fn name(&self) -> &str {
        "run_tests"
    }

    fn risk(&self, _input: &str) -> ToolRisk {
        ToolRisk::Execute
    }

    fn call(&self, input: &str, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        let command = if input.trim().is_empty() {
            "cargo test"
        } else {
            input.trim()
        };
        shell_output(command, &context.workspace_root, self.timeout)
    }
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
        shell_output(input, &context.workspace_root, self.timeout)
    }
}

fn workspace_path(root: &Path, input: &str) -> Result<PathBuf, ToolError> {
    let root = root
        .canonicalize()
        .map_err(|error| ToolError::new(format!("invalid workspace root: {error}")))?;
    let path = root.join(input.trim());
    let canonical = path
        .canonicalize()
        .map_err(|error| ToolError::new(format!("invalid path: {error}")))?;
    if !canonical.starts_with(root) {
        return Err(ToolError::new("path escapes workspace"));
    }
    Ok(canonical)
}

fn existing_workspace_path(root: &Path, input: &str) -> Result<PathBuf, ToolError> {
    let path = writable_workspace_path(root, input)?;
    path.canonicalize()
        .map_err(|error| ToolError::new(format!("invalid path: {error}")))
}

fn writable_workspace_path(root: &Path, input: &str) -> Result<PathBuf, ToolError> {
    let root = root
        .canonicalize()
        .map_err(|error| ToolError::new(format!("invalid workspace root: {error}")))?;
    let relative = Path::new(input.trim());
    if relative.is_absolute() {
        return Err(ToolError::new("path escapes workspace"));
    }
    if relative
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(ToolError::new("path escapes workspace"));
    }
    let path = root.join(relative);
    let parent = path
        .parent()
        .ok_or_else(|| ToolError::new("invalid path"))?
        .canonicalize()
        .map_err(|error| ToolError::new(format!("invalid parent path: {error}")))?;
    if !parent.starts_with(root) {
        return Err(ToolError::new("path escapes workspace"));
    }
    Ok(path)
}

fn shell_output(command: &str, root: &Path, timeout: Duration) -> Result<ToolOutput, ToolError> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(root)
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
        if started.elapsed() >= timeout {
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

fn command_output(
    command: &str,
    args: &[&str],
    root: &Path,
    timeout: Option<Duration>,
) -> Result<ToolOutput, ToolError> {
    let command_text = std::iter::once(command)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    shell_output(
        &command_text,
        root,
        timeout.unwrap_or(Duration::from_secs(120)),
    )
}

struct ReplacePatch<'a> {
    path: &'a str,
    find: &'a str,
    replace: &'a str,
}

impl<'a> ReplacePatch<'a> {
    fn parse(input: &'a str) -> Result<Self, ToolError> {
        let Some((path, rest)) = input.split_once("\n---FIND---\n") else {
            return Err(ToolError::new(
                "apply_patch input must include ---FIND--- section",
            ));
        };
        let Some((find, replace)) = rest.split_once("\n---REPLACE---\n") else {
            return Err(ToolError::new(
                "apply_patch input must include ---REPLACE--- section",
            ));
        };
        Ok(Self {
            path: path.trim(),
            find,
            replace,
        })
    }
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
    fn apply_patch_should_replace_text_inside_workspace() {
        let root = temp_dir("apply_patch");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> i32 { 41 }\n").unwrap();
        let tool = ApplyPatchTool;

        let output = tool
            .call(
                "src/lib.rs\n---FIND---\n41\n---REPLACE---\n42",
                &ToolContext {
                    workspace_root: root,
                },
            )
            .unwrap();

        assert!(output.stdout.contains("patched src/lib.rs"));
    }

    #[test]
    fn apply_patch_should_report_find_text_missing() {
        let root = temp_dir("apply_patch_missing");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> i32 { 42 }\n").unwrap();
        let tool = ApplyPatchTool;

        let error = tool
            .call(
                "src/lib.rs\n---FIND---\n41\n---REPLACE---\n42",
                &ToolContext {
                    workspace_root: root,
                },
            )
            .unwrap_err();

        assert_eq!(error.message, "apply_patch failed: find text not found");
    }

    #[test]
    fn write_file_should_reject_parent_dir_escape() {
        let root = temp_dir("write_escape_parent");
        fs::create_dir_all(&root).unwrap();
        let tool = WriteFileTool;

        let error = tool
            .call(
                "../outside.txt\n---CONTENT---\nnope",
                &ToolContext {
                    workspace_root: root,
                },
            )
            .unwrap_err();

        assert_eq!(error.message, "path escapes workspace");
    }

    #[test]
    fn write_file_should_reject_symlink_escape() {
        let root = temp_dir("write_escape_symlink");
        let outside = temp_dir("outside_dir");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let tool = WriteFileTool;

        let error = tool
            .call(
                "link/outside.txt\n---CONTENT---\nnope",
                &ToolContext {
                    workspace_root: root,
                },
            )
            .unwrap_err();

        assert_eq!(error.message, "path escapes workspace");
    }

    #[test]
    fn write_file_should_reject_absolute_escape() {
        let root = temp_dir("write_escape_absolute");
        fs::create_dir_all(&root).unwrap();
        let tool = WriteFileTool;

        let error = tool
            .call(
                "/tmp/outside.txt\n---CONTENT---\nnope",
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

    #[test]
    fn run_tests_should_preserve_failure_output() {
        let root = temp_dir("run_tests_failure");
        fs::create_dir_all(&root).unwrap();
        let tool = RunTestsTool::default();

        let output = tool
            .call(
                "printf failure >&2; exit 1",
                &ToolContext {
                    workspace_root: root,
                },
            )
            .unwrap();

        assert!(output.stderr.contains("failure"));
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("flash_tools_{name}_{nanos}"))
    }
}
