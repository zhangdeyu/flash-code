use std::path::Path;
use std::process::Command;

use flash_core::{ToolError, ToolErrorKind};

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

/// Build a sandboxed `/bin/sh` command using macOS `sandbox-exec`.
#[cfg(target_os = "macos")]
pub(super) fn sandboxed_shell_command(sandbox_exec: &Path) -> Result<Command, ToolError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn missing_sandbox_exec_should_be_rejected_before_spawn() {
        let missing = super::super::super::test_support::temp_dir("missing_sandbox_exec");

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
}
