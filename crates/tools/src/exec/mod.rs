use std::time::Duration;

use flash_core::{Tool, ToolContext, ToolError, ToolErrorKind, ToolOutput, ToolRisk};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::parse_input;

mod output;
mod policy_hint;
mod process;
mod sandbox;

pub use sandbox::{macos_sandbox_status, SandboxStatus};

pub struct BashTool {
    pub(crate) timeout: Duration,
    pub(crate) max_output_bytes: usize,
    pub(crate) allow_network: bool,
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
        Ok(policy_hint::command_risk(&input.command))
    }

    fn call(&self, input: Value, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        let input: BashInput = parse_input(input)?;
        let timeout = input
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(self.timeout);
        let risk = policy_hint::command_risk(&input.command);
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
        process::shell_output(
            &input.command,
            &context.workspace_root,
            timeout,
            self.max_output_bytes,
            &context.cancellation,
            self.allow_network,
            output::output_artifacts(context)?,
        )
    }
}

#[derive(Debug, Deserialize)]
struct BashInput {
    command: String,
    timeout_secs: Option<u64>,
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::test_support::{context, temp_dir};
    use flash_core::{ArtifactLimits, ToolExitStatus};

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
    fn read_pid(path: &std::path::Path) -> i32 {
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
}
