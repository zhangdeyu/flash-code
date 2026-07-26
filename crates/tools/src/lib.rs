use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Component;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use flash_core::{
    ArtifactLimits, CancellationToken, Tool, ToolContext, ToolError, ToolErrorKind, ToolExitStatus,
    ToolOutput, ToolRegistry, ToolRisk,
};
use serde::Deserialize;
use serde_json::{json, Value};

pub fn builtin_registry() -> Result<ToolRegistry, flash_core::tools::ToolRegistryError> {
    builtin_registry_with_options(120, 200_000, false)
}

pub fn builtin_registry_with_options(
    shell_timeout_secs: u64,
    shell_max_output_bytes: usize,
    allow_network: bool,
) -> Result<ToolRegistry, flash_core::tools::ToolRegistryError> {
    if !allow_network {
        let sandbox = macos_sandbox_status();
        if !sandbox.available {
            return Err(flash_core::tools::ToolRegistryError::RuntimeUnavailable(
                format!(
                    "network-disabled Bash requires macOS sandbox-exec: {}",
                    sandbox.detail
                ),
            ));
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxStatus {
    pub available: bool,
    pub detail: String,
}

pub fn macos_sandbox_status() -> SandboxStatus {
    #[cfg(target_os = "macos")]
    {
        sandbox_status_with_path(Path::new("/usr/bin/sandbox-exec"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        SandboxStatus {
            available: false,
            detail: "unsupported operating system".to_string(),
        }
    }
}

#[cfg(target_os = "macos")]
fn sandbox_status_with_path(path: &Path) -> SandboxStatus {
    let result = Command::new(path)
        .arg("-p")
        .arg("(version 1) (allow default) (deny network*)")
        .arg("/usr/bin/true")
        .status();
    match result {
        Ok(status) if status.success() => SandboxStatus {
            available: true,
            detail: path.display().to_string(),
        },
        Ok(status) => SandboxStatus {
            available: false,
            detail: format!("{} exited with {status}", path.display()),
        },
        Err(error) => SandboxStatus {
            available: false,
            detail: format!("{}: {error}", path.display()),
        },
    }
}

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
        visit_files(&context.workspace_root, &mut |path| {
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
        visit_files(&context.workspace_root, &mut |path| {
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

pub struct BashTool {
    timeout: Duration,
    max_output_bytes: usize,
    allow_network: bool,
}

impl Default for BashTool {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
            max_output_bytes: 200_000,
            allow_network: false,
        }
    }
}

impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        "Execute a shell command in the workspace directory. When network access is disabled, only explicitly allowlisted offline command forms are accepted."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to execute"},
                "timeout_secs": {"type": "integer", "minimum": 1, "description": "Optional timeout in seconds (default 120)"}
            },
            "required": ["command"]
        })
    }

    fn risk(&self, input: &Value) -> Result<ToolRisk, ToolError> {
        let input: BashInput = parse_input(input.clone())?;
        Ok(command_risk(&input.command))
    }

    fn call(&self, input: Value, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        let input: BashInput = parse_input(input)?;
        let timeout = input
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(self.timeout);
        let risk = command_risk(&input.command);
        if !self.allow_network && risk != ToolRisk::Read {
            return Err(ToolError::with_kind(
                ToolErrorKind::PermissionDenied,
                "Bash command is not in the offline allowlist while network access is disabled",
            ));
        }
        if context.cancellation.is_cancelled() {
            return Err(ToolError::with_kind(
                ToolErrorKind::Cancelled,
                "Bash cancelled before execution",
            ));
        }
        shell_output(
            &input.command,
            &context.workspace_root,
            timeout,
            self.max_output_bytes,
            &context.cancellation,
            self.allow_network,
            output_artifacts(context)?,
        )
    }
}

#[derive(Debug, Deserialize)]
struct ReadInput {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct EditInput {
    path: String,
    find: String,
    replace: String,
}

#[derive(Debug, Deserialize)]
struct WriteInput {
    path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct GlobInput {
    pattern: String,
}

#[derive(Debug, Deserialize)]
struct GrepInput {
    pattern: String,
}

#[derive(Debug, Deserialize)]
struct ListFilesInput {
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BashInput {
    command: String,
    timeout_secs: Option<u64>,
}

fn parse_input<T: for<'de> Deserialize<'de>>(input: Value) -> Result<T, ToolError> {
    serde_json::from_value(input).map_err(|error| {
        ToolError::with_kind(
            ToolErrorKind::InvalidInput,
            format!("invalid tool input: {error}"),
        )
    })
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

fn command_risk(input: &str) -> ToolRisk {
    let command = input.trim().to_ascii_lowercase();
    if contains_command(
        &command,
        &["curl", "wget", "nc", "ncat", "ssh", "scp", "ftp"],
    ) || command.contains("http://")
        || command.contains("https://")
    {
        ToolRisk::Network
    } else if command.contains("rm -rf")
        || command.starts_with("rm ")
        || command.contains("git clean")
        || command.contains("git reset --hard")
        || command.contains("find ") && command.contains("-delete")
        || command.contains("shutdown")
        || command.contains("mkfs")
        || command.contains("rmtree")
    {
        ToolRisk::Destructive
    } else if !command.contains([';', '|', '>', '<', '`'])
        && !command.contains("$(")
        && is_allowlisted_command(&command)
    {
        ToolRisk::Read
    } else {
        ToolRisk::Destructive
    }
}

fn contains_command(command: &str, names: &[&str]) -> bool {
    command
        .split(|character: char| character.is_whitespace() || ";|&()".contains(character))
        .any(|token| names.contains(&token))
}

fn is_allowlisted_command(command: &str) -> bool {
    [
        "pwd",
        "ls",
        "rg",
        "git status",
        "git diff",
        "git log",
        "git show",
        "cargo test",
        "cargo check",
        "cargo clippy",
        "cargo fmt",
    ]
    .iter()
    .any(|allowed| command == *allowed || command.starts_with(&format!("{allowed} ")))
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
    let root = root.canonicalize().map_err(|error| {
        ToolError::with_kind(
            ToolErrorKind::Io,
            format!("invalid workspace root: {error}"),
        )
    })?;
    let path = root.join(input.trim());
    let canonical = path.canonicalize().map_err(|error| {
        ToolError::with_kind(ToolErrorKind::Io, format!("invalid path: {error}"))
    })?;
    if !canonical.starts_with(root) {
        return Err(ToolError::with_kind(
            ToolErrorKind::PermissionDenied,
            "path escapes workspace",
        ));
    }
    Ok(canonical)
}

fn existing_workspace_path(root: &Path, input: &str) -> Result<PathBuf, ToolError> {
    let path = writable_workspace_path(root, input)?;
    path.canonicalize()
        .map_err(|error| ToolError::with_kind(ToolErrorKind::Io, format!("invalid path: {error}")))
}

fn writable_workspace_path(root: &Path, input: &str) -> Result<PathBuf, ToolError> {
    let root = root.canonicalize().map_err(|error| {
        ToolError::with_kind(
            ToolErrorKind::Io,
            format!("invalid workspace root: {error}"),
        )
    })?;
    let relative = Path::new(input.trim());
    if relative.is_absolute() {
        return Err(ToolError::with_kind(
            ToolErrorKind::PermissionDenied,
            "path escapes workspace",
        ));
    }
    if relative
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(ToolError::with_kind(
            ToolErrorKind::PermissionDenied,
            "path escapes workspace",
        ));
    }
    let path = root.join(relative);
    let parent = path
        .parent()
        .ok_or_else(|| ToolError::with_kind(ToolErrorKind::InvalidInput, "invalid path"))?
        .canonicalize()
        .map_err(|error| {
            ToolError::with_kind(ToolErrorKind::Io, format!("invalid parent path: {error}"))
        })?;
    if !parent.starts_with(root) {
        return Err(ToolError::with_kind(
            ToolErrorKind::PermissionDenied,
            "path escapes workspace",
        ));
    }
    Ok(path)
}

fn shell_output(
    command: &str,
    root: &Path,
    timeout: Duration,
    max_output_bytes: usize,
    cancellation: &CancellationToken,
    allow_network: bool,
    artifacts: Option<ShellArtifacts>,
) -> Result<ToolOutput, ToolError> {
    let mut process = shell_command(allow_network)?;
    process
        .arg("-c")
        .arg(command)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !allow_network {
        process
            .env("CARGO_NET_OFFLINE", "true")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("ALL_PROXY", "socks5://127.0.0.1:9")
            .env("NO_PROXY", "");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        process.process_group(0);
    }
    let mut child = process.spawn().map_err(|error| {
        ToolError::with_kind(ToolErrorKind::Io, format!("failed to spawn shell: {error}"))
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        ToolError::with_kind(ToolErrorKind::Internal, "shell stdout pipe is missing")
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        ToolError::with_kind(ToolErrorKind::Internal, "shell stderr pipe is missing")
    })?;
    let stdout_artifact = artifacts.as_ref().map(|artifacts| artifacts.stdout.clone());
    let stderr_artifact = artifacts.map(|artifacts| artifacts.stderr);
    let stdout_reader =
        thread::spawn(move || read_limited(stdout, max_output_bytes, stdout_artifact));
    let stderr_reader =
        thread::spawn(move || read_limited(stderr, max_output_bytes, stderr_artifact));

    let started = Instant::now();
    let (exit_status, timed_out, cancelled) = loop {
        if let Some(status) = child.try_wait().map_err(|error| {
            ToolError::with_kind(ToolErrorKind::Io, format!("failed to poll shell: {error}"))
        })? {
            break (status, false, false);
        }
        if cancellation.is_cancelled() {
            terminate_process_group(&mut child)?;
            let status = child.wait().map_err(|error| {
                ToolError::with_kind(
                    ToolErrorKind::Io,
                    format!("failed to reap cancelled shell: {error}"),
                )
            })?;
            break (status, false, true);
        }
        if started.elapsed() >= timeout {
            terminate_process_group(&mut child)?;
            let status = child.wait().map_err(|error| {
                ToolError::with_kind(
                    ToolErrorKind::Io,
                    format!("failed to reap timed out shell: {error}"),
                )
            })?;
            break (status, true, false);
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout_reader.join().map_err(|_| {
        ToolError::with_kind(ToolErrorKind::Internal, "shell stdout reader panicked")
    })?;
    let stderr = stderr_reader.join().map_err(|_| {
        ToolError::with_kind(ToolErrorKind::Internal, "shell stderr reader panicked")
    })?;
    let stdout = stdout?;
    let mut stderr = stderr?;
    if timed_out {
        stderr
            .preview
            .extend_from_slice(b"\nshell command timed out");
    } else if cancelled {
        stderr
            .preview
            .extend_from_slice(b"\nshell command cancelled");
    }
    let artifact = [stdout.artifact, stderr.artifact]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
    Ok(ToolOutput {
        stdout: String::from_utf8_lossy(&stdout.preview).to_string(),
        stderr: String::from_utf8_lossy(&stderr.preview).to_string(),
        status: if cancelled {
            ToolExitStatus::Cancelled
        } else if exit_status.success() && !timed_out {
            ToolExitStatus::Success
        } else {
            ToolExitStatus::Error
        },
        exit_code: exit_status.code(),
        signal: exit_signal(exit_status),
        duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        timed_out,
        truncated: stdout.truncated || stderr.truncated,
        artifact: (!artifact.is_empty()).then_some(artifact),
    })
}

fn shell_command(allow_network: bool) -> Result<Command, ToolError> {
    #[cfg(target_os = "macos")]
    if !allow_network {
        return sandboxed_shell_command(Path::new("/usr/bin/sandbox-exec"));
    }
    #[cfg(not(target_os = "macos"))]
    if !allow_network {
        return Err(ToolError::with_kind(
            ToolErrorKind::PermissionDenied,
            "network-disabled Bash requires macOS sandbox-exec",
        ));
    }
    Ok(Command::new("/bin/sh"))
}

#[cfg(target_os = "macos")]
fn sandboxed_shell_command(sandbox_exec: &Path) -> Result<Command, ToolError> {
    let status = sandbox_status_with_path(sandbox_exec);
    if !status.available {
        return Err(ToolError::with_kind(
            ToolErrorKind::PermissionDenied,
            format!("macOS sandbox unavailable: {}", status.detail),
        ));
    }
    let mut command = Command::new(sandbox_exec);
    command
        .arg("-p")
        .arg("(version 1) (allow default) (deny network*)")
        .arg("/bin/sh");
    Ok(command)
}

#[derive(Clone)]
struct OutputArtifact {
    path: PathBuf,
    reference: String,
    limits: ArtifactLimits,
    usage: ArtifactUsage,
}

struct ShellArtifacts {
    stdout: OutputArtifact,
    stderr: OutputArtifact,
}

struct CapturedOutput {
    preview: Vec<u8>,
    truncated: bool,
    artifact: Option<String>,
}

#[derive(Clone)]
struct ArtifactUsage {
    used: Arc<Mutex<u64>>,
}

impl ArtifactUsage {
    fn reserve(&self, bytes: u64, max_total: u64) -> Result<(), ToolError> {
        let mut used = self
            .used
            .lock()
            .map_err(|_| ToolError::with_kind(ToolErrorKind::Internal, "artifact budget lock"))?;
        if used.saturating_add(bytes) > max_total {
            return Err(ToolError::with_kind(
                ToolErrorKind::ResourceLimit,
                format!("session artifacts exceeded {max_total} bytes"),
            ));
        }
        *used += bytes;
        Ok(())
    }

    fn release(&self, bytes: u64) {
        if let Ok(mut used) = self.used.lock() {
            *used = used.saturating_sub(bytes);
        }
    }
}

fn output_artifacts(context: &ToolContext) -> Result<Option<ShellArtifacts>, ToolError> {
    let (Some(dir), Some(stem)) = (&context.artifact_dir, &context.artifact_stem) else {
        return Ok(None);
    };
    if stem.is_empty()
        || !stem
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(ToolError::with_kind(
            ToolErrorKind::InvalidInput,
            "invalid artifact stem",
        ));
    }
    let limits = context.artifact_limits.unwrap_or_default();
    let usage = ArtifactUsage {
        used: Arc::new(Mutex::new(artifact_directory_usage(dir)?)),
    };
    Ok(Some(ShellArtifacts {
        stdout: OutputArtifact {
            path: dir.join(format!("{stem}.stdout.txt")),
            reference: format!("artifacts/{stem}.stdout.txt"),
            limits,
            usage: usage.clone(),
        },
        stderr: OutputArtifact {
            path: dir.join(format!("{stem}.stderr.txt")),
            reference: format!("artifacts/{stem}.stderr.txt"),
            limits,
            usage,
        },
    }))
}

fn artifact_directory_usage(dir: &Path) -> Result<u64, ToolError> {
    let mut total = 0_u64;
    for entry in fs::read_dir(dir).map_err(|error| {
        ToolError::with_kind(
            ToolErrorKind::Io,
            format!("failed to inspect artifact directory: {error}"),
        )
    })? {
        let entry = entry.map_err(|error| {
            ToolError::with_kind(
                ToolErrorKind::Io,
                format!("failed to inspect artifact: {error}"),
            )
        })?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
            ToolError::with_kind(
                ToolErrorKind::Io,
                format!("failed to inspect artifact: {error}"),
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ToolError::with_kind(
                ToolErrorKind::PermissionDenied,
                "artifact directory contains an untrusted non-file entry",
            ));
        }
        total = total.saturating_add(metadata.len());
    }
    Ok(total)
}

struct ArtifactWriter {
    file: File,
    artifact: OutputArtifact,
    written: u64,
}

impl ArtifactWriter {
    fn create(artifact: OutputArtifact) -> Result<Self, ToolError> {
        let file = File::create_new(&artifact.path).map_err(|error| {
            ToolError::with_kind(
                ToolErrorKind::Io,
                format!("failed to create shell output artifact: {error}"),
            )
        })?;
        Ok(Self {
            file,
            artifact,
            written: 0,
        })
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), ToolError> {
        let bytes_len = bytes.len() as u64;
        if self.written.saturating_add(bytes_len) > self.artifact.limits.max_file_bytes {
            return Err(self.fail(format!(
                "artifact exceeded {} bytes",
                self.artifact.limits.max_file_bytes
            )));
        }
        if let Err(error) = self
            .artifact
            .usage
            .reserve(bytes_len, self.artifact.limits.max_session_bytes)
        {
            self.cleanup();
            return Err(error);
        }
        if let Err(error) = self.file.write_all(bytes) {
            self.artifact.usage.release(bytes_len);
            self.cleanup();
            return Err(ToolError::with_kind(
                ToolErrorKind::Io,
                format!("failed to write shell output artifact: {error}"),
            ));
        }
        self.written += bytes_len;
        Ok(())
    }

    fn fail(&mut self, message: String) -> ToolError {
        self.cleanup();
        ToolError::with_kind(ToolErrorKind::ResourceLimit, message)
    }

    fn cleanup(&mut self) {
        self.artifact.usage.release(self.written);
        self.written = 0;
        let _result = fs::remove_file(&self.artifact.path);
    }
}

fn read_limited(
    mut reader: impl Read,
    max_output_bytes: usize,
    artifact: Option<OutputArtifact>,
) -> Result<CapturedOutput, ToolError> {
    let mut preview = Vec::with_capacity(max_output_bytes.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    let mut truncated = false;
    let mut artifact_writer: Option<ArtifactWriter> = None;
    let mut artifact_reference = None;
    loop {
        let read = reader.read(&mut buffer).map_err(|error| {
            ToolError::with_kind(
                ToolErrorKind::Io,
                format!("failed to read shell output: {error}"),
            )
        })?;
        if read == 0 {
            break;
        }
        let remaining = max_output_bytes.saturating_sub(preview.len());
        let keep = read.min(remaining);
        preview.extend_from_slice(&buffer[..keep]);
        if let Some(writer) = artifact_writer.as_mut() {
            writer.write_all(&buffer[..read])?;
        } else if keep < read {
            truncated = true;
            if let Some(artifact) = &artifact {
                let mut writer = ArtifactWriter::create(artifact.clone())?;
                writer.write_all(&preview)?;
                writer.write_all(&buffer[keep..read])?;
                artifact_reference = Some(artifact.reference.clone());
                artifact_writer = Some(writer);
            }
        }
    }
    Ok(CapturedOutput {
        preview,
        truncated,
        artifact: artifact_reference,
    })
}

#[cfg(unix)]
fn terminate_process_group(child: &mut Child) -> Result<(), ToolError> {
    let process_group = -(child.id() as i32);
    // SAFETY: the child was placed in its own process group before spawning.
    let result = unsafe { libc::kill(process_group, libc::SIGKILL) };
    if result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(ToolError::with_kind(
            ToolErrorKind::Io,
            format!(
                "failed to terminate shell process group: {}",
                std::io::Error::last_os_error()
            ),
        ))
    }
}

#[cfg(not(unix))]
fn terminate_process_group(child: &mut Child) -> Result<(), ToolError> {
    child.kill().map_err(|error| {
        ToolError::with_kind(
            ToolErrorKind::Io,
            format!("failed to terminate shell process: {error}"),
        )
    })
}

#[cfg(unix)]
fn exit_signal(status: ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|signal| signal.to_string())
}

#[cfg(not(unix))]
fn exit_signal(_status: ExitStatus) -> Option<String> {
    None
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

    #[test]
    fn list_files_should_list_directory_entries() {
        let root = temp_dir("list_files");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "").unwrap();
        let tool = ListFilesTool;

        let output = tool.call(json!({"path": "."}), &context(root)).unwrap();

        assert!(output.stdout.contains("src/"));
    }

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

    #[test]
    fn bash_should_execute_successful_command() {
        let root = temp_dir("bash_success");
        fs::create_dir_all(&root).unwrap();
        let tool = BashTool {
            timeout: Duration::from_secs(1),
            allow_network: true,
            ..BashTool::default()
        };

        let output = tool
            .call(json!({"command": "printf ok"}), &context(root))
            .unwrap();

        assert_eq!(output.stdout, "ok");
    }

    #[test]
    fn bash_should_return_error_for_failing_command() {
        let root = temp_dir("bash_failure");
        fs::create_dir_all(&root).unwrap();
        let tool = BashTool {
            timeout: Duration::from_secs(1),
            allow_network: true,
            ..BashTool::default()
        };

        let output = tool
            .call(
                json!({"command": "printf nope >&2; exit 7"}),
                &context(root),
            )
            .unwrap();

        assert_eq!(output.status, ToolExitStatus::Error);
    }

    #[test]
    fn bash_risk_should_classify_destructive_commands() {
        let tool = BashTool::default();

        assert_eq!(
            tool.risk(&json!({"command": "rm -rf target"})).unwrap(),
            ToolRisk::Destructive
        );
    }

    #[test]
    fn bash_should_return_error_on_timeout() {
        let root = temp_dir("shell_timeout");
        fs::create_dir_all(&root).unwrap();
        let tool = BashTool {
            timeout: Duration::from_millis(100),
            allow_network: true,
            ..BashTool::default()
        };

        let output = tool
            .call(
                json!({"command": "sleep 30 & echo $! > child.pid; wait"}),
                &context(root.clone()),
            )
            .unwrap();

        assert_eq!(output.status, ToolExitStatus::Error);
        assert!(output.timed_out);
        assert_process_exited(read_pid(&root.join("child.pid")));
    }

    #[test]
    fn bash_should_drain_large_output_without_deadlock() {
        let root = temp_dir("bash_large_output");
        fs::create_dir_all(&root).unwrap();
        let artifact_dir = root.join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let tool = BashTool {
            timeout: Duration::from_secs(5),
            max_output_bytes: 1_024,
            allow_network: true,
        };
        let mut context = context(root);
        context.artifact_dir = Some(artifact_dir.clone());
        context.artifact_stem = Some("call_large".to_string());
        context.artifact_limits = Some(ArtifactLimits {
            max_file_bytes: 400_000,
            max_session_bytes: 400_000,
        });

        let output = tool
            .call(json!({"command": "yes x | head -c 300000"}), &context)
            .unwrap();

        assert_eq!(output.stdout.len(), 1_024);
        assert!(output.truncated);
        assert_eq!(
            output.artifact.as_deref(),
            Some("artifacts/call_large.stdout.txt")
        );
        assert_eq!(
            fs::metadata(artifact_dir.join("call_large.stdout.txt"))
                .unwrap()
                .len(),
            300_000
        );
    }

    #[test]
    fn bash_should_cancel_running_process_group() {
        let root = temp_dir("bash_cancel");
        fs::create_dir_all(&root).unwrap();
        let tool = BashTool {
            timeout: Duration::from_secs(10),
            allow_network: true,
            ..BashTool::default()
        };
        let context = context(root);
        let cancellation = context.cancellation.clone();
        let cancel_thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            cancellation.cancel();
        });

        let output = tool
            .call(
                json!({"command": "sleep 30 & echo $! > child.pid; wait"}),
                &context,
            )
            .unwrap();
        cancel_thread.join().unwrap();

        assert_eq!(output.status, ToolExitStatus::Cancelled);
        assert!(output.duration_ms < 2_000);
        assert_process_exited(read_pid(&context.workspace_root.join("child.pid")));
    }

    #[test]
    fn bash_should_deny_network_command_when_network_is_disabled() {
        let root = temp_dir("bash_network_disabled");
        fs::create_dir_all(&root).unwrap();
        let tool = BashTool::default();

        let error = tool
            .call(
                json!({"command": "curl https://example.com"}),
                &context(root),
            )
            .unwrap_err();

        assert_eq!(error.kind, ToolErrorKind::PermissionDenied);
    }

    #[test]
    fn bash_risk_should_require_approval_for_unrecognized_command() {
        let tool = BashTool::default();

        let risk = tool.risk(&json!({"command": "python script.py"})).unwrap();

        assert_eq!(risk, ToolRisk::Destructive);
    }

    #[test]
    fn bash_should_deny_interpreter_indirection_when_network_is_disabled() {
        let root = temp_dir("bash_interpreter_network_disabled");
        fs::create_dir_all(&root).unwrap();
        let tool = BashTool::default();

        let error = tool
            .call(
                json!({"command": "python -c 'import urllib.request'"}),
                &context(root),
            )
            .unwrap_err();

        assert_eq!(error.kind, ToolErrorKind::PermissionDenied);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sandboxed_shell_should_block_real_socket_connection() {
        let root = temp_dir("sandbox_socket");
        fs::create_dir_all(&root).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept = thread::spawn(move || listener.accept().unwrap());
        let allowed = shell_output(
            &format!("/usr/bin/nc -z 127.0.0.1 {port}"),
            &root,
            Duration::from_secs(2),
            1024,
            &CancellationToken::new(),
            true,
            None,
        )
        .unwrap();
        accept.join().unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let denied = shell_output(
            &format!("/usr/bin/nc -z 127.0.0.1 {port}"),
            &root,
            Duration::from_secs(2),
            1024,
            &CancellationToken::new(),
            false,
            None,
        )
        .unwrap();

        assert_eq!(allowed.status, ToolExitStatus::Success);
        assert_eq!(denied.status, ToolExitStatus::Error);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn missing_sandbox_exec_should_be_rejected_before_spawn() {
        let missing = temp_dir("missing_sandbox_exec");

        let error = sandboxed_shell_command(&missing).unwrap_err();

        assert_eq!(error.kind, ToolErrorKind::PermissionDenied);
        assert!(error.message.contains("sandbox unavailable"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn startup_sandbox_probe_should_report_available_runtime() {
        let status = macos_sandbox_status();

        assert!(status.available, "{}", status.detail);
        assert_eq!(status.detail, "/usr/bin/sandbox-exec");
    }

    #[test]
    fn shell_artifact_limits_should_stop_single_and_total_writes() {
        let root = temp_dir("shell_artifact_limits");
        let artifact_dir = root.join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let tool = BashTool {
            timeout: Duration::from_secs(2),
            max_output_bytes: 2,
            allow_network: true,
        };
        let mut single = context(root.clone());
        single.artifact_dir = Some(artifact_dir.clone());
        single.artifact_stem = Some("single".to_string());
        single.artifact_limits = Some(ArtifactLimits {
            max_file_bytes: 4,
            max_session_bytes: 20,
        });

        let single_error = tool
            .call(json!({"command": "printf 12345678"}), &single)
            .unwrap_err();
        assert_eq!(single_error.kind, ToolErrorKind::ResourceLimit);
        assert_eq!(fs::read_dir(&artifact_dir).unwrap().count(), 0);

        let mut total = context(root);
        total.artifact_dir = Some(artifact_dir.clone());
        total.artifact_stem = Some("total".to_string());
        total.artifact_limits = Some(ArtifactLimits {
            max_file_bytes: 10,
            max_session_bytes: 12,
        });
        let total_error = tool
            .call(
                json!({"command": "printf 12345678; printf abcdefgh >&2"}),
                &total,
            )
            .unwrap_err();

        assert_eq!(total_error.kind, ToolErrorKind::ResourceLimit);
        let total_bytes = fs::read_dir(artifact_dir)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum::<u64>();
        assert!(total_bytes <= 12);
    }

    #[cfg(unix)]
    #[test]
    fn shell_artifact_should_reject_symlink_entries() {
        let root = temp_dir("shell_artifact_symlink");
        let artifact_dir = root.join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        std::os::unix::fs::symlink("/tmp", artifact_dir.join("untrusted")).unwrap();
        let mut context = context(root);
        context.artifact_dir = Some(artifact_dir);
        context.artifact_stem = Some("call".to_string());
        context.artifact_limits = Some(ArtifactLimits::default());

        let error = match output_artifacts(&context) {
            Ok(_) => panic!("symlink artifact entry should be rejected"),
            Err(error) => error,
        };

        assert_eq!(error.kind, ToolErrorKind::PermissionDenied);
    }

    fn context(workspace_root: PathBuf) -> ToolContext {
        ToolContext {
            workspace_root,
            cancellation: CancellationToken::new(),
            artifact_dir: None,
            artifact_stem: None,
            artifact_limits: None,
        }
    }

    #[cfg(unix)]
    fn read_pid(path: &Path) -> i32 {
        for _ in 0..100 {
            if let Ok(content) = fs::read_to_string(path) {
                return content.trim().parse().unwrap();
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("child PID file was not created");
    }

    #[cfg(unix)]
    fn assert_process_exited(pid: i32) {
        for _ in 0..100 {
            let result = unsafe { libc::kill(pid, 0) };
            if result != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("descendant process {pid} is still alive");
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("flash_tools_{name}_{nanos}"))
    }
}
