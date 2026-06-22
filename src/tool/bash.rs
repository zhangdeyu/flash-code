use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::protocol::ToolSpec;
use crate::tool::{Tool, ToolOutcome};

/// Execute shell commands via `sh -c`.
///
/// Uses `Command::spawn()` + holds the `Child` handle so cancel can `kill()` it.
pub struct BashTool;

async fn read_handle(handle: Option<tokio::process::ChildStdout>) -> String {
    let Some(mut h) = handle else {
        return String::new();
    };
    let mut buf = String::new();
    let _ = h.read_to_string(&mut buf).await;
    buf
}

async fn read_stderr_handle(handle: Option<tokio::process::ChildStderr>) -> String {
    let Some(mut h) = handle else {
        return String::new();
    };
    let mut buf = String::new();
    let _ = h.read_to_string(&mut buf).await;
    buf
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "Execute a shell command".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" }
                },
                "required": ["command"]
            }),
        }
    }

    async fn run(&self, input: serde_json::Value, cancel: CancellationToken) -> ToolOutcome {
        let cmd = input["command"].as_str().unwrap_or_default();

        let mut child = match Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => return ToolOutcome::Failure(format!("failed to spawn: {e}")),
        };

        // Take stdout/stderr handles before waiting, so we can read after wait.
        let stdout_handle = child.stdout.take();
        let stderr_handle = child.stderr.take();

        tokio::select! {
            status = child.wait() => {
                let stdout = read_handle(stdout_handle).await;
                let stderr = read_stderr_handle(stderr_handle).await;

                match status {
                    Ok(s) if s.success() => {
                        ToolOutcome::Success(serde_json::json!({"stdout": stdout}))
                    }
                    Ok(s) => {
                        ToolOutcome::Failure(format!("exit {}: stderr={stderr} stdout={stdout}",
                            s.code().unwrap_or(-1)))
                    }
                    Err(e) => ToolOutcome::Failure(e.to_string()),
                }
            }
            _ = cancel.cancelled() => {
                // Explicitly kill the child process
                let _ = child.kill().await;
                ToolOutcome::Cancelled
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn successful_command() {
        let tool = BashTool;
        let cancel = CancellationToken::new();
        let result = tool.run(serde_json::json!({"command": "echo hello"}), cancel).await;
        match result {
            ToolOutcome::Success(v) => assert!(v["stdout"].as_str().is_some_and(|s| s.contains("hello"))),
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn failing_command() {
        let tool = BashTool;
        let cancel = CancellationToken::new();
        let result = tool.run(serde_json::json!({"command": "exit 1"}), cancel).await;
        assert!(matches!(result, ToolOutcome::Failure(_)));
    }

    #[tokio::test]
    async fn cancel_kills_process() {
        let tool = BashTool;
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        // Start a long-running command and cancel it immediately
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel_clone.cancel();
        });

        let result = tool.run(serde_json::json!({"command": "sleep 60"}), cancel).await;
        assert!(matches!(result, ToolOutcome::Cancelled));
    }
}
// BashTool: spawn + kill on cancel
