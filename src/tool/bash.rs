use std::process::Stdio;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::process::Command;

use crate::protocol::{ApprovalDecision, RiskLevel, ToolApprovalAdvice, ToolSpec};
use crate::tool::{ExecutionContext, Tool, ToolOutput};

pub struct BashTool;

#[derive(Deserialize, JsonSchema)]
struct BashInput {
    /// The shell command to execute via `sh -c`.
    command: String,
}

fn truncate(s: String, max: usize) -> String {
    if s.len() <= max {
        s
    } else {
        let mut out = s;
        out.truncate(max);
        out.push_str("\n...[truncated]");
        out
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn spec(&self) -> ToolSpec {
        let schema = schemars::schema_for!(BashInput);
        ToolSpec {
            name: "bash".into(),
            description:
                "Execute a shell command via `sh -c`. Output truncated to ctx.max_output_bytes."
                    .into(),
            input_schema: serde_json::to_value(schema).unwrap_or(serde_json::json!({})),
            risk: RiskLevel::Dangerous,
        }
    }

    fn approval_advice(&self, input: &serde_json::Value) -> ToolApprovalAdvice {
        let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
        if cmd.contains("rm -rf") {
            return ToolApprovalAdvice {
                decision: ApprovalDecision::MustAsk,
                reason: Some("destructive: rm -rf detected".into()),
            };
        }
        ToolApprovalAdvice::default_for(RiskLevel::Dangerous)
    }

    async fn run(&self, input: serde_json::Value, ctx: &ExecutionContext) -> ToolOutput {
        let parsed: BashInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolOutput::failure_invalid_input(e),
        };

        let child = Command::new("sh")
            .arg("-c")
            .arg(&parsed.command)
            .current_dir(&ctx.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let child = match child {
            Ok(c) => c,
            Err(e) => return ToolOutput::failure(format!("spawn failed: {e}")),
        };

        let out = match child.wait_with_output().await {
            Ok(o) => o,
            Err(e) => return ToolOutput::failure(e.to_string()),
        };

        let stdout = truncate(
            String::from_utf8_lossy(&out.stdout).into_owned(),
            ctx.max_output_bytes,
        );
        let stderr = truncate(
            String::from_utf8_lossy(&out.stderr).into_owned(),
            ctx.max_output_bytes,
        );

        if out.status.success() {
            ToolOutput::text(stdout)
        } else {
            let code = out.status.code().unwrap_or(-1);
            ToolOutput::failure(format!("exit {code}: {stderr}"))
        }
    }
}
