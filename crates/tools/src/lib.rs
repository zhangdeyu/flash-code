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
    registry.register(Box::new(ReadTool))?;
    registry.register(Box::new(EditTool))?;
    registry.register(Box::new(WriteTool))?;
    registry.register(Box::new(GlobTool))?;
    registry.register(Box::new(GrepTool))?;
    registry.register(Box::new(ListFilesTool))?;
    registry.register(Box::new(BashTool::default()))?;
    registry.register(Box::new(SearchTool))?;
    registry.register(Box::new(ReadFileTool))?;
    registry.register(Box::new(ShellTool::default()))?;
    registry.register(Box::new(ApplyPatchTool))?;
    registry.register(Box::new(WriteFileTool))?;
    registry.register(Box::new(GitDiffTool))?;
    registry.register(Box::new(RunTestsTool::default()))?;
    Ok(registry)
}

pub struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read the full or partial content of a file within the workspace."
    }

    fn parameters(&self) -> &str {
        r#"{"type":"object","properties":{"path":{"type":"string","description":"Relative path to the file to read"},"start_line":{"type":"integer","description":"First line to read (1-indexed, inclusive)"},"end_line":{"type":"integer","description":"Last line to read (1-indexed, inclusive)"}},"required":["path"]}""
        "#
    }

    fn risk(&self, _input: &str) -> ToolRisk {
        ToolRisk::Read
    }

    fn call(&self, input: &str, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        read_file(input, context)
    }
}

pub struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> &str {
        "Make a targeted find-and-replace edit to an existing file. The find text must match exactly."
    }

    fn parameters(&self) -> &str {
        r#"{"type":"object","properties":{"path":{"type":"string","description":"Relative path to the file to edit"},"find":{"type":"string","description":"Exact text to find in the file"},"replace":{"type":"string","description":"Replacement text"}},"required":["path","find","replace"]}""
        "#
    }

    fn risk(&self, _input: &str) -> ToolRisk {
        ToolRisk::Write
    }

    fn call(&self, input: &str, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        apply_replace_patch(input, context)
    }
}

pub struct WriteTool;

impl Tool for WriteTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn description(&self) -> &str {
        "Create a new file or completely overwrite an existing file with the given content."
    }

    fn parameters(&self) -> &str {
        r#"{"type":"object","properties":{"path":{"type":"string","description":"Relative path to the file to write"},"content":{"type":"string","description":"Full content to write to the file"}},"required":["path","content"]}""
        "#
    }

    fn risk(&self, _input: &str) -> ToolRisk {
        ToolRisk::Write
    }

    fn call(&self, input: &str, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        write_file(input, context)
    }
}

pub struct GlobTool;

impl Tool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &str {
        "Find files and directories matching a glob pattern (e.g. `**/*.rs`, `src/*.toml`)."
    }

    fn parameters(&self) -> &str {
        r#"{"type":"object","properties":{"pattern":{"type":"string","description":"Glob pattern to match against relative file paths"}},"required":["pattern"]}""
        "#
    }

    fn risk(&self, _input: &str) -> ToolRisk {
        ToolRisk::Read
    }

    fn call(&self, input: &str, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        let pattern = input.trim();
        let mut matches = Vec::new();
        visit_files(&context.workspace_root, &mut |path| {
            if matches.len() >= 200 {
                return;
            }
            let relative = path.strip_prefix(&context.workspace_root).unwrap_or(path);
            let relative_text = relative.display().to_string();
            if glob_match(pattern, &relative_text) {
                matches.push(relative_text);
            }
        })?;
        Ok(ToolOutput::success(matches.join("\n")))
    }
}

pub struct GrepTool;

impl Tool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &str {
        "Search for a keyword or pattern in file contents across the workspace. Returns file:line:content matches."
    }

    fn parameters(&self) -> &str {
        r#"{"type":"object","properties":{"pattern":{"type":"string","description":"Keyword or substring to search for in file contents"}},"required":["pattern"]}""
        "#
    }

    fn risk(&self, _input: &str) -> ToolRisk {
        ToolRisk::Read
    }

    fn call(&self, input: &str, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        let needle = input.trim();
        let mut matches = Vec::new();
        visit_files(&context.workspace_root, &mut |path| {
            if matches.len() >= 200 {
                return;
            }
            let Ok(content) = fs::read_to_string(path) else {
                return;
            };
            for (line_index, line) in content.lines().enumerate() {
                if line.contains(needle) {
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

pub struct ListFilesTool;

impl Tool for ListFilesTool {
    fn name(&self) -> &str {
        "ListFiles"
    }

    fn description(&self) -> &str {
        "List the immediate children (files and directories) of a path in the workspace."
    }

    fn parameters(&self) -> &str {
        r#"{"type":"object","properties":{"path":{"type":"string","description":"Relative path to the directory to list. Defaults to workspace root if omitted."}},"required":[]}""
        "#
    }

    fn risk(&self, _input: &str) -> ToolRisk {
        ToolRisk::Read
    }

    fn call(&self, input: &str, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        let path = if input.trim().is_empty() || input.trim() == "." {
            context.workspace_root.clone()
        } else {
            workspace_path(&context.workspace_root, input)?
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

pub struct BashTool {
    timeout: Duration,
}

impl Default for BashTool {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
        }
    }
}

impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        "Execute a shell command in the workspace directory. Use for running tests, git commands, builds, etc."
    }

    fn parameters(&self) -> &str {
        r#"{"type":"object","properties":{"command":{"type":"string","description":"Shell command to execute"},"timeout_secs":{"type":"integer","description":"Optional timeout in seconds (default 120)"}},"required":["command"]}""
        "#
    }

    fn risk(&self, input: &str) -> ToolRisk {
        command_risk(input)
    }

    fn call(&self, input: &str, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        shell_output(input, &context.workspace_root, self.timeout)
    }
}

pub struct SearchTool;

impl Tool for SearchTool {
    fn name(&self) -> &str {
        "search"
    }

    fn description(&self) -> &str {
        "[Legacy] Search for files by name in the workspace."
    }

    fn parameters(&self) -> &str {
        r#"{"type":"object","properties":{"query":{"type":"string","description":"Filename or path fragment to search for"}},"required":[]}""
        "#
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

    fn description(&self) -> &str {
        "[Legacy] Read the content of a file."
    }

    fn parameters(&self) -> &str {
        r#"{"type":"object","properties":{"path":{"type":"string","description":"Path to the file"}},"required":["path"]}""
        "#
    }

    fn risk(&self, _input: &str) -> ToolRisk {
        ToolRisk::Read
    }

    fn call(&self, input: &str, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        read_file(input, context)
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

    fn description(&self) -> &str {
        "[Legacy] Apply a find-and-replace patch to a file."
    }

    fn parameters(&self) -> &str {
        r#"{"type":"object","properties":{"input":{"type":"string","description":"Patch input in legacy format"}},"required":["input"]}""
        "#
    }

    fn risk(&self, _input: &str) -> ToolRisk {
        ToolRisk::Write
    }

    fn call(&self, input: &str, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        apply_replace_patch(input, context)
    }
}

pub struct WriteFileTool;

impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "[Legacy] Write content to a file."
    }

    fn parameters(&self) -> &str {
        r#"{"type":"object","properties":{"input":{"type":"string","description":"Path and content in legacy format"}},"required":["input"]}""
        "#
    }

    fn risk(&self, _input: &str) -> ToolRisk {
        ToolRisk::Write
    }

    fn call(&self, input: &str, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        write_file(input, context)
    }
}

pub struct GitDiffTool;

impl Tool for GitDiffTool {
    fn name(&self) -> &str {
        "git_diff"
    }

    fn description(&self) -> &str {
        "[Legacy] Show the current git diff for the workspace."
    }

    fn parameters(&self) -> &str {
        r#"{"type":"object","properties":{},"required":[]}""
        "#
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

    fn description(&self) -> &str {
        "[Legacy] Run tests using cargo test or a custom command."
    }

    fn parameters(&self) -> &str {
        r#"{"type":"object","properties":{"command":{"type":"string","description":"Test command to run (defaults to `cargo test`)"}},"required":[]}""
        "#
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

    fn description(&self) -> &str {
        "[Legacy] Execute a shell command in the workspace directory."
    }

    fn parameters(&self) -> &str {
        r#"{"type":"object","properties":{"command":{"type":"string","description":"Shell command to execute"}},"required":["command"]}""
        "#
    }

    fn risk(&self, input: &str) -> ToolRisk {
        command_risk(input)
    }

    fn call(&self, input: &str, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        shell_output(input, &context.workspace_root, self.timeout)
    }
}

fn read_file(input: &str, context: &ToolContext) -> Result<ToolOutput, ToolError> {
    let path = workspace_path(&context.workspace_root, input)?;
    let content = fs::read_to_string(path)
        .map_err(|error| ToolError::new(format!("failed to read file: {error}")))?;
    Ok(ToolOutput::success(content))
}

fn apply_replace_patch(input: &str, context: &ToolContext) -> Result<ToolOutput, ToolError> {
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

fn write_file(input: &str, context: &ToolContext) -> Result<ToolOutput, ToolError> {
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

fn command_risk(input: &str) -> ToolRisk {
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
    fn builtin_registry_should_expose_new_protocol_names_and_legacy_aliases() {
        let registry = builtin_registry().unwrap();

        for name in [
            "Read",
            "Edit",
            "Write",
            "Glob",
            "Grep",
            "ListFiles",
            "Bash",
            "read_file",
            "apply_patch",
            "write_file",
            "search",
            "shell",
            "git_diff",
            "run_tests",
        ] {
            assert!(registry.get(name).is_some(), "missing tool {name}");
        }
    }

    #[test]
    fn v1_tool_risks_should_match_protocol() {
        let registry = builtin_registry().unwrap();
        let cases = [
            ("Read", "src/lib.rs", ToolRisk::Read),
            (
                "Edit",
                "src/lib.rs\n---FIND---\na\n---REPLACE---\nb",
                ToolRisk::Write,
            ),
            ("Write", "src/lib.rs\n---CONTENT---\n", ToolRisk::Write),
            ("Glob", "**/*.rs", ToolRisk::Read),
            ("Grep", "answer", ToolRisk::Read),
            ("ListFiles", ".", ToolRisk::Read),
            ("Bash", "cargo test", ToolRisk::Execute),
            ("Bash", "rm -rf target", ToolRisk::Destructive),
        ];

        for (name, input, expected) in cases {
            let tool = registry.get(name).unwrap();
            assert_eq!(tool.risk(input), expected, "wrong risk for {name}");
        }
    }

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
    fn read_should_read_file_contents() {
        let root = temp_dir("read_success");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn ok() {}").unwrap();
        let tool = ReadTool;

        let output = tool.call("src/lib.rs", &context(root)).unwrap();

        assert_eq!(output.stdout, "pub fn ok() {}");
    }

    #[test]
    fn read_should_report_missing_file() {
        let root = temp_dir("read_missing");
        fs::create_dir_all(&root).unwrap();
        let tool = ReadTool;

        let error = tool.call("missing.rs", &context(root)).unwrap_err();

        assert!(error.message.contains("invalid path"));
    }

    #[test]
    fn list_files_should_list_directory_entries() {
        let root = temp_dir("list_files");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "").unwrap();
        let tool = ListFilesTool;

        let output = tool.call(".", &context(root)).unwrap();

        assert!(output.stdout.contains("src/"));
    }

    #[test]
    fn list_files_should_report_missing_directory() {
        let root = temp_dir("list_files_missing");
        fs::create_dir_all(&root).unwrap();
        let tool = ListFilesTool;

        let error = tool.call("missing", &context(root)).unwrap_err();

        assert!(error.message.contains("invalid path"));
    }

    #[test]
    fn glob_should_match_file_patterns() {
        let root = temp_dir("glob");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        let tool = GlobTool;

        let output = tool.call("**/*.rs", &context(root)).unwrap();

        assert!(output.stdout.contains("src/lib.rs"));
    }

    #[test]
    fn glob_should_report_invalid_workspace_root() {
        let root = temp_dir("glob_missing_root");
        let tool = GlobTool;

        let error = tool.call("**/*.rs", &context(root)).unwrap_err();

        assert!(error.message.contains("failed to read dir"));
    }

    #[test]
    fn grep_should_search_file_contents() {
        let root = temp_dir("grep");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> i32 { 42 }").unwrap();
        let tool = GrepTool;

        let output = tool.call("answer", &context(root)).unwrap();

        assert!(output.stdout.contains("src/lib.rs:1"));
    }

    #[test]
    fn grep_should_report_invalid_workspace_root() {
        let root = temp_dir("grep_missing_root");
        let tool = GrepTool;

        let error = tool.call("answer", &context(root)).unwrap_err();

        assert!(error.message.contains("failed to read dir"));
    }

    #[test]
    fn read_file_should_reject_workspace_escape() {
        let root = temp_dir("read_escape");
        fs::create_dir_all(&root).unwrap();
        let outside = temp_dir("outside");
        fs::write(&outside, "secret").unwrap();
        let tool = ReadFileTool;

        let error = tool
            .call(outside.to_str().unwrap(), &context(root))
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
                &context(root),
            )
            .unwrap();

        assert!(output.stdout.contains("patched src/lib.rs"));
    }

    #[test]
    fn edit_should_replace_text_inside_workspace() {
        let root = temp_dir("edit");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> i32 { 41 }\n").unwrap();
        let tool = EditTool;

        let output = tool
            .call(
                "src/lib.rs\n---FIND---\n41\n---REPLACE---\n42",
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
                "src/lib.rs\n---FIND---\n41\n---REPLACE---\n42",
                &context(root),
            )
            .unwrap_err();

        assert_eq!(error.message, "apply_patch failed: find text not found");
    }

    #[test]
    fn edit_should_reject_parent_dir_escape() {
        let root = temp_dir("edit_escape_parent");
        fs::create_dir_all(&root).unwrap();
        let tool = EditTool;

        let error = tool
            .call(
                "../outside.rs\n---FIND---\nold\n---REPLACE---\nnew",
                &context(root),
            )
            .unwrap_err();

        assert_eq!(error.message, "path escapes workspace");
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
                &context(root),
            )
            .unwrap_err();

        assert_eq!(error.message, "apply_patch failed: find text not found");
    }

    #[test]
    fn write_should_create_file_contents() {
        let root = temp_dir("write_success");
        fs::create_dir_all(root.join("src")).unwrap();
        let tool = WriteTool;

        let output = tool
            .call(
                "src/lib.rs\n---CONTENT---\npub fn ok() {}",
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
    fn write_should_report_invalid_input_shape() {
        let root = temp_dir("write_invalid");
        fs::create_dir_all(&root).unwrap();
        let tool = WriteTool;

        let error = tool.call("src/lib.rs", &context(root)).unwrap_err();

        assert!(error.message.contains("write_file input must be"));
    }

    #[test]
    fn write_file_should_reject_parent_dir_escape() {
        let root = temp_dir("write_escape_parent");
        fs::create_dir_all(&root).unwrap();
        let tool = WriteFileTool;

        let error = tool
            .call("../outside.txt\n---CONTENT---\nnope", &context(root))
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
            .call("link/outside.txt\n---CONTENT---\nnope", &context(root))
            .unwrap_err();

        assert_eq!(error.message, "path escapes workspace");
    }

    #[test]
    fn write_file_should_reject_absolute_escape() {
        let root = temp_dir("write_escape_absolute");
        fs::create_dir_all(&root).unwrap();
        let tool = WriteFileTool;

        let error = tool
            .call("/tmp/outside.txt\n---CONTENT---\nnope", &context(root))
            .unwrap_err();

        assert_eq!(error.message, "path escapes workspace");
    }

    #[test]
    fn bash_should_execute_successful_command() {
        let root = temp_dir("bash_success");
        fs::create_dir_all(&root).unwrap();
        let tool = BashTool {
            timeout: Duration::from_secs(1),
        };

        let output = tool.call("printf ok", &context(root)).unwrap();

        assert_eq!(output.stdout, "ok");
    }

    #[test]
    fn bash_should_return_error_for_failing_command() {
        let root = temp_dir("bash_failure");
        fs::create_dir_all(&root).unwrap();
        let tool = BashTool {
            timeout: Duration::from_secs(1),
        };

        let output = tool
            .call("printf nope >&2; exit 7", &context(root))
            .unwrap();

        assert_eq!(output.status, ToolExitStatus::Error);
    }

    #[test]
    fn bash_risk_should_classify_destructive_commands() {
        let tool = BashTool::default();

        assert_eq!(tool.risk("rm -rf target"), ToolRisk::Destructive);
    }

    #[test]
    fn shell_should_return_error_on_timeout() {
        let root = temp_dir("shell_timeout");
        fs::create_dir_all(&root).unwrap();
        let tool = ShellTool {
            timeout: Duration::from_millis(1),
        };

        let output = tool.call("sleep 1", &context(root)).unwrap();

        assert_eq!(output.status, ToolExitStatus::Error);
    }

    #[test]
    fn run_tests_should_preserve_failure_output() {
        let root = temp_dir("run_tests_failure");
        fs::create_dir_all(&root).unwrap();
        let tool = RunTestsTool::default();

        let output = tool
            .call("printf failure >&2; exit 1", &context(root))
            .unwrap();

        assert!(output.stderr.contains("failure"));
    }

    fn context(workspace_root: PathBuf) -> ToolContext {
        ToolContext { workspace_root }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("flash_tools_{name}_{nanos}"))
    }
}
