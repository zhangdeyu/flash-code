use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use flash_core::{CancellationToken, ToolError, ToolErrorKind, ToolExitStatus, ToolOutput};

use super::output::{read_limited, CapturedOutput, ShellArtifacts};

/// Execute a shell command, capturing bounded stdout/stderr and honoring
/// cancellation/timeout. Excess output is spilled into artifact files.
pub(super) fn shell_output(
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
    let stdout: CapturedOutput = stdout?;
    let mut stderr: CapturedOutput = stderr?;
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
        return super::sandbox::sandboxed_shell_command(Path::new("/usr/bin/sandbox-exec"));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_dir;
    use std::fs;

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
}
