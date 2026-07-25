use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use flash_agent::{AgentOptions, AgentRuntime, SmokeProvider};
use flash_core::{init_workspace, Outcome, PermissionPolicy, ToolExitStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalTask {
    pub id: String,
    pub instruction: String,
    pub kind: EvalTaskKind,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalTaskKind {
    LocalRustFixture,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalRun {
    pub id: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalResult {
    pub task_id: String,
    pub passed: bool,
    pub duration_ms: u128,
    pub session_id: Option<String>,
    pub events_path: Option<PathBuf>,
    pub workspace_path: PathBuf,
    pub failure_kind: Option<EvalFailureKind>,
    pub failure_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalFailureKind {
    AgentFailure,
    EnvironmentFailure,
    GraderFailure,
}

impl EvalFailureKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentFailure => "agent_failure",
            Self::EnvironmentFailure => "environment_failure",
            Self::GraderFailure => "grader_failure",
        }
    }
}

#[derive(Debug)]
pub enum EvalError {
    Io(std::io::Error),
    Storage(flash_core::storage::StorageError),
    Agent(flash_agent::AgentError),
    ToolRegistry(flash_core::tools::ToolRegistryError),
    InvalidTask(String),
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "eval io error: {error}"),
            Self::Storage(error) => write!(formatter, "{error}"),
            Self::Agent(error) => write!(formatter, "{error}"),
            Self::ToolRegistry(error) => write!(formatter, "{error}"),
            Self::InvalidTask(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for EvalError {}

impl From<std::io::Error> for EvalError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<flash_core::storage::StorageError> for EvalError {
    fn from(error: flash_core::storage::StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<flash_agent::AgentError> for EvalError {
    fn from(error: flash_agent::AgentError) -> Self {
        Self::Agent(error)
    }
}

impl From<flash_core::tools::ToolRegistryError> for EvalError {
    fn from(error: flash_core::tools::ToolRegistryError) -> Self {
        Self::ToolRegistry(error)
    }
}

pub fn local_fixture_task(task_id: &str) -> Result<EvalTask, EvalError> {
    match task_id {
        "fix-rust" => Ok(EvalTask {
            id: "fix-rust".to_string(),
            instruction: "fix failing tests".to_string(),
            kind: EvalTaskKind::LocalRustFixture,
            timeout_secs: 120,
        }),
        _ => Err(EvalError::InvalidTask(format!(
            "unknown fixture eval task `{task_id}`"
        ))),
    }
}

pub fn run_fixture_eval(root: &Path, task: EvalTask) -> Result<EvalResult, EvalError> {
    let run = create_eval_run(root)?;
    let result = run_task_in_eval_run(&run, &task)?;
    write_result_json(&run, &result)?;
    write_report_markdown(&run, &result)?;
    Ok(result)
}

fn create_eval_run(root: &Path) -> Result<EvalRun, EvalError> {
    let id = format!("eval_{}", timestamp_nanos());
    let path = root.join(".flash").join("evals").join(&id);
    fs::create_dir_all(&path)?;
    Ok(EvalRun { id, path })
}

fn run_task_in_eval_run(run: &EvalRun, task: &EvalTask) -> Result<EvalResult, EvalError> {
    let started = Instant::now();
    let workspace_path = run.path.join("workspace");
    fs::create_dir_all(&workspace_path)?;
    let setup_result = match task.kind {
        EvalTaskKind::LocalRustFixture => setup_rust_fixture(&workspace_path),
    };
    if let Err(error) = setup_result {
        let result = EvalResult {
            task_id: task.id.clone(),
            passed: false,
            duration_ms: started.elapsed().as_millis(),
            session_id: None,
            events_path: None,
            workspace_path,
            failure_kind: Some(EvalFailureKind::EnvironmentFailure),
            failure_reason: Some(error.to_string()),
        };
        return Ok(result);
    }
    init_workspace(&workspace_path)?;
    let mut runtime = AgentRuntime::new(
        SmokeProvider::new(),
        flash_tools::builtin_registry()?,
        AgentOptions {
            model: "smoke".to_string(),
            max_turns: 8,
            permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Yolo),
            max_output_bytes: 200_000,
            max_prompt_bytes: 200_000,
        },
    );
    let agent_run = match runtime.run_task(&workspace_path, &task.instruction) {
        Ok(run) => run,
        Err(error) => {
            return Ok(EvalResult {
                task_id: task.id.clone(),
                passed: false,
                duration_ms: started.elapsed().as_millis(),
                session_id: None,
                events_path: None,
                workspace_path,
                failure_kind: Some(EvalFailureKind::AgentFailure),
                failure_reason: Some(error.to_string()),
            });
        }
    };
    let events_path = workspace_path
        .join(".flash")
        .join("sessions")
        .join(&agent_run.session_id)
        .join("events.jsonl");
    let grader = run_grader(&workspace_path, run)?;
    let passed =
        agent_run.outcome == Outcome::Succeeded && grader.status == ToolExitStatus::Success;
    let failure_kind = if passed {
        None
    } else if agent_run.outcome != Outcome::Succeeded {
        Some(EvalFailureKind::AgentFailure)
    } else {
        Some(EvalFailureKind::GraderFailure)
    };
    let failure_reason = if passed {
        None
    } else {
        Some(format!(
            "agent_outcome={}, grader_status={:?}",
            agent_run.outcome.as_str(),
            grader.status
        ))
    };
    Ok(EvalResult {
        task_id: task.id.clone(),
        passed,
        duration_ms: started.elapsed().as_millis(),
        session_id: Some(agent_run.session_id),
        events_path: Some(events_path),
        workspace_path,
        failure_kind,
        failure_reason,
    })
}

fn setup_rust_fixture(root: &Path) -> Result<(), EvalError> {
    fs::create_dir_all(root.join("src"))?;
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"flash_eval_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )?;
    fs::write(
        root.join("src/lib.rs"),
        "pub fn answer() -> i32 {\n    41\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn answer_should_be_42() {\n        assert_eq!(answer(), 42);\n    }\n}\n",
    )?;
    run_command(root, "git", &["init"])?;
    run_command(root, "git", &["add", "."])?;
    let output = Command::new("git")
        .args(["commit", "-m", "baseline"])
        .env("GIT_AUTHOR_NAME", "Flash Eval")
        .env("GIT_AUTHOR_EMAIL", "flash-eval@example.com")
        .env("GIT_COMMITTER_NAME", "Flash Eval")
        .env("GIT_COMMITTER_EMAIL", "flash-eval@example.com")
        .current_dir(root)
        .output()?;
    if !output.status.success() {
        return Err(EvalError::InvalidTask(format!(
            "failed to create fixture git baseline: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

fn run_grader(root: &Path, run: &EvalRun) -> Result<flash_core::ToolOutput, EvalError> {
    let output = Command::new("cargo")
        .arg("test")
        .current_dir(root)
        .output()?;
    let status = if output.status.success() {
        ToolExitStatus::Success
    } else {
        ToolExitStatus::Error
    };
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    fs::write(run.path.join("grader.stdout.txt"), &stdout)?;
    fs::write(run.path.join("grader.stderr.txt"), &stderr)?;
    Ok(flash_core::ToolOutput {
        stdout,
        stderr,
        status,
    })
}

fn run_command(root: &Path, command: &str, args: &[&str]) -> Result<(), EvalError> {
    let output = Command::new(command)
        .args(args)
        .current_dir(root)
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(EvalError::InvalidTask(format!(
        "command `{command}` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    )))
}

fn write_result_json(run: &EvalRun, result: &EvalResult) -> Result<(), EvalError> {
    let events_path = result
        .events_path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let session_id = result.session_id.clone().unwrap_or_default();
    let failure_kind = result
        .failure_kind
        .map(EvalFailureKind::as_str)
        .unwrap_or_default();
    let failure_reason = result.failure_reason.clone().unwrap_or_default();
    let content = format!(
        concat!(
            "{{",
            "\"task_id\":\"{}\",",
            "\"passed\":{},",
            "\"duration_ms\":{},",
            "\"session_id\":\"{}\",",
            "\"events_path\":\"{}\",",
            "\"workspace_path\":\"{}\",",
            "\"failure_kind\":\"{}\",",
            "\"failure_reason\":\"{}\"",
            "}}\n"
        ),
        escape_json(&result.task_id),
        result.passed,
        result.duration_ms,
        escape_json(&session_id),
        escape_json(&events_path),
        escape_json(&result.workspace_path.display().to_string()),
        escape_json(failure_kind),
        escape_json(&failure_reason)
    );
    fs::write(run.path.join("result.json"), content)?;
    Ok(())
}

fn write_report_markdown(run: &EvalRun, result: &EvalResult) -> Result<(), EvalError> {
    let events_path = result
        .events_path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "n/a".to_string());
    let session_id = result.session_id.as_deref().unwrap_or("n/a");
    let failure_kind = result
        .failure_kind
        .map(EvalFailureKind::as_str)
        .unwrap_or("none");
    let failure_reason = result.failure_reason.as_deref().unwrap_or("none");
    let content = format!(
        concat!(
            "# Flash Eval Report\n\n",
            "| Field | Value |\n",
            "|---|---|\n",
            "| task id | {} |\n",
            "| passed | {} |\n",
            "| duration ms | {} |\n",
            "| session id | {} |\n",
            "| events path | {} |\n",
            "| failure kind | {} |\n",
            "| failure reason | {} |\n"
        ),
        result.task_id,
        result.passed,
        result.duration_ms,
        session_id,
        events_path,
        failure_kind,
        failure_reason
    );
    fs::write(run.path.join("report.md"), content)?;
    Ok(())
}

fn escape_json(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if ch.is_control() => escaped.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => escaped.push(ch),
        }
    }
    escaped
}

fn timestamp_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_nanos()
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn run_fixture_eval_should_write_result_report_and_events() {
        let root = temp_dir("fixture_eval");
        fs::create_dir_all(&root).unwrap();

        let result = run_fixture_eval(&root, local_fixture_task("fix-rust").unwrap()).unwrap();

        assert!(result.passed);
        assert!(result.events_path.as_ref().unwrap().exists());
        assert!(result.workspace_path.join("src/lib.rs").exists());
        let eval_dir = result
            .workspace_path
            .parent()
            .expect("workspace has eval parent");
        assert!(eval_dir.join("result.json").exists());
        assert!(eval_dir.join("report.md").exists());
        assert!(eval_dir.join("grader.stdout.txt").exists());
        assert!(eval_dir.join("grader.stderr.txt").exists());
    }

    #[test]
    fn local_fixture_task_should_reject_unknown_task() {
        let error = local_fixture_task("missing").unwrap_err();

        assert!(error.to_string().contains("unknown fixture eval task"));
    }

    #[test]
    fn eval_failure_kind_should_have_stable_json_names() {
        assert_eq!(EvalFailureKind::AgentFailure.as_str(), "agent_failure");
        assert_eq!(
            EvalFailureKind::EnvironmentFailure.as_str(),
            "environment_failure"
        );
        assert_eq!(EvalFailureKind::GraderFailure.as_str(), "grader_failure");
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("flash_eval_{name}_{nanos}"))
    }
}
