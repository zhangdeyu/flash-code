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
    pub command_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub session_id: Option<String>,
    pub events_path: Option<PathBuf>,
    pub workspace_path: PathBuf,
    pub failure_kind: Option<EvalFailureKind>,
    pub failure_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalBenchRun {
    pub run_id: String,
    pub path: PathBuf,
    pub subset: String,
    pub lock_version: String,
    pub results: Vec<EvalResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweBenchTask {
    pub instance_id: String,
    pub repo: String,
    pub base_commit: String,
    pub version: String,
    pub problem_statement: String,
    pub fail_to_pass: Vec<String>,
    pub pass_to_pass: Vec<String>,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweBenchRun {
    pub run_id: String,
    pub path: PathBuf,
    pub subset: String,
    pub limit: usize,
    pub lock_version: String,
    pub results: Vec<SweBenchResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweBenchResult {
    pub instance_id: String,
    pub resolved: bool,
    pub duration_ms: u128,
    pub command_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub session_id: Option<String>,
    pub events_path: Option<PathBuf>,
    pub patch_path: Option<PathBuf>,
    pub workspace_path: PathBuf,
    pub failure_kind: Option<SweBenchFailureKind>,
    pub failure_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweBenchSummary {
    pub total: usize,
    pub evaluated: usize,
    pub resolved: usize,
    pub unresolved: usize,
    pub environment_failures: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegressionRun {
    pub run_id: String,
    pub path: PathBuf,
    pub report_path: PathBuf,
    pub result_path: PathBuf,
    pub trend_path: PathBuf,
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub pass_rate_bps: u64,
    pub previous_pass_rate_bps: Option<u64>,
    pub new_failures: Vec<String>,
    pub benchmarks: Vec<RegressionBenchmarkResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegressionBenchmarkResult {
    pub benchmark: String,
    pub subset: String,
    pub lock_version: String,
    pub total: usize,
    pub passed: usize,
    pub agent_failures: usize,
    pub environment_failures: usize,
    pub benchmark_failures: usize,
    pub report_path: PathBuf,
    pub result_path: PathBuf,
    pub replay_paths: Vec<PathBuf>,
    pub failures: Vec<RegressionFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegressionFailure {
    pub id: String,
    pub category: RegressionFailureCategory,
    pub reason: String,
    pub replay_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegressionFailureCategory {
    Agent,
    Environment,
    Benchmark,
}

impl RegressionFailureCategory {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent_failure",
            Self::Environment => "environment_failure",
            Self::Benchmark => "benchmark_failure",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweBenchFailureKind {
    LocalizationFailure,
    PatchFailure,
    TestFailure,
    EnvironmentFailure,
    Timeout,
}

impl SweBenchFailureKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LocalizationFailure => "localization_failure",
            Self::PatchFailure => "patch_failure",
            Self::TestFailure => "test_failure",
            Self::EnvironmentFailure => "environment_failure",
            Self::Timeout => "timeout",
        }
    }
}

impl SweBenchRun {
    pub fn summary(&self) -> SweBenchSummary {
        swe_bench_summary_counts(&self.results)
    }
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

pub fn terminal_bench_smoke_tasks() -> Result<Vec<EvalTask>, EvalError> {
    parse_terminal_bench_smoke_lock(include_str!("../fixtures/terminal_bench_smoke.lock"))
}

pub fn swe_bench_verified_tasks() -> Result<Vec<SweBenchTask>, EvalError> {
    parse_swe_bench_verified_lock(include_str!("../fixtures/swe_bench_verified_smoke.lock"))
}

pub async fn run_fixture_eval(root: &Path, task: EvalTask) -> Result<EvalResult, EvalError> {
    let run = create_eval_run(root)?;
    let result = run_task_in_eval_run(&run, &task).await?;
    write_result_json(&run, &result)?;
    write_report_markdown(&run, &result)?;
    Ok(result)
}

pub async fn run_terminal_bench_smoke(root: &Path) -> Result<TerminalBenchRun, EvalError> {
    let run = create_eval_run(root)?;
    let lock_content = include_str!("../fixtures/terminal_bench_smoke.lock");
    fs::write(run.path.join("terminal_bench_smoke.lock"), lock_content)?;
    let tasks = parse_terminal_bench_smoke_lock(lock_content)?;
    let mut results = Vec::new();
    for task in tasks {
        let task_run = EvalRun {
            id: format!("{}_{}", run.id, sanitize_id(&task.id)),
            path: run.path.join("tasks").join(sanitize_id(&task.id)),
        };
        fs::create_dir_all(&task_run.path)?;
        let result = run_task_in_eval_run(&task_run, &task).await?;
        write_result_json(&task_run, &result)?;
        write_report_markdown(&task_run, &result)?;
        results.push(result);
    }
    let terminal_run = TerminalBenchRun {
        run_id: run.id,
        path: run.path,
        subset: "smoke".to_string(),
        lock_version: terminal_bench_lock_value(lock_content, "lock_version")
            .unwrap_or_else(|| "unknown".to_string()),
        results,
    };
    write_terminal_bench_summary(&terminal_run)?;
    Ok(terminal_run)
}

pub async fn run_swe_bench_verified(root: &Path, limit: usize) -> Result<SweBenchRun, EvalError> {
    let run = create_eval_run(root)?;
    let lock_content = include_str!("../fixtures/swe_bench_verified_smoke.lock");
    fs::write(run.path.join("swe_bench_verified_smoke.lock"), lock_content)?;
    let tasks = parse_swe_bench_verified_lock(lock_content)?;
    let limit = limit.min(tasks.len());
    let mut results = Vec::new();
    for task in tasks.into_iter().take(limit) {
        let task_run = EvalRun {
            id: format!("{}_{}", run.id, sanitize_id(&task.instance_id)),
            path: run.path.join("tasks").join(sanitize_id(&task.instance_id)),
        };
        fs::create_dir_all(&task_run.path)?;
        let result = run_swe_bench_task(&task_run, &task).await?;
        write_swe_bench_task_result(&task_run, &result)?;
        results.push(result);
    }
    let swe_run = SweBenchRun {
        run_id: run.id,
        path: run.path,
        subset: "verified".to_string(),
        limit,
        lock_version: terminal_bench_lock_value(lock_content, "lock_version")
            .unwrap_or_else(|| "unknown".to_string()),
        results,
    };
    write_swe_bench_summary(&swe_run)?;
    Ok(swe_run)
}

pub async fn run_regression(root: &Path) -> Result<RegressionRun, EvalError> {
    run_regression_with_swe_limit(root, 10).await
}

async fn run_regression_with_swe_limit(
    root: &Path,
    swe_limit: usize,
) -> Result<RegressionRun, EvalError> {
    let run = create_eval_run(root)?;
    let previous = latest_previous_regression(root, &run.path)?;
    let fixture = run_fixture_eval(root, local_fixture_task("fix-rust")?).await?;
    let terminal = run_terminal_bench_smoke(root).await?;
    let swe = run_swe_bench_verified(root, swe_limit).await?;
    let benchmarks = vec![
        regression_from_fixture(&fixture),
        regression_from_terminal_bench(&terminal),
        regression_from_swe_bench(&swe),
    ];
    let total = benchmarks
        .iter()
        .map(|benchmark| benchmark.total)
        .sum::<usize>();
    let passed = benchmarks
        .iter()
        .map(|benchmark| benchmark.passed)
        .sum::<usize>();
    let failed = total.saturating_sub(passed);
    let pass_rate_bps = pass_rate_bps(passed, total);
    let failures = regression_failure_ids(&benchmarks);
    let previous_failures = previous
        .as_ref()
        .map(|snapshot| snapshot.failures.as_slice())
        .unwrap_or_default();
    let new_failures = new_failure_ids(&failures, previous_failures);
    let regression = RegressionRun {
        run_id: run.id,
        report_path: run.path.join("report.md"),
        result_path: run.path.join("regression_result.json"),
        trend_path: run.path.join("trend.json"),
        path: run.path,
        total,
        passed,
        failed,
        pass_rate_bps,
        previous_pass_rate_bps: previous.map(|snapshot| snapshot.pass_rate_bps),
        new_failures,
        benchmarks,
    };
    write_regression_summary(&regression)?;
    Ok(regression)
}

fn create_eval_run(root: &Path) -> Result<EvalRun, EvalError> {
    let id = format!("eval_{}", timestamp_nanos());
    let path = root.join(".flash").join("evals").join(&id);
    fs::create_dir_all(&path)?;
    Ok(EvalRun { id, path })
}

async fn run_task_in_eval_run(run: &EvalRun, task: &EvalTask) -> Result<EvalResult, EvalError> {
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
            command_count: 0,
            input_tokens: 0,
            output_tokens: 0,
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
    let agent_run = match runtime.run_task(&workspace_path, &task.instruction).await {
        Ok(run) => run,
        Err(error) => {
            return Ok(EvalResult {
                task_id: task.id.clone(),
                passed: false,
                duration_ms: started.elapsed().as_millis(),
                command_count: 0,
                input_tokens: 0,
                output_tokens: 0,
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
    let metrics = read_event_metrics(&events_path)?;
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
        command_count: metrics.command_count,
        input_tokens: metrics.input_tokens,
        output_tokens: metrics.output_tokens,
        session_id: Some(agent_run.session_id),
        events_path: Some(events_path),
        workspace_path,
        failure_kind,
        failure_reason,
    })
}

fn parse_terminal_bench_smoke_lock(content: &str) -> Result<Vec<EvalTask>, EvalError> {
    let mut tasks = Vec::new();
    for line in content.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || !line.starts_with("task=") {
            continue;
        }
        let body = line.trim_start_matches("task=");
        let mut id = String::new();
        let mut instruction = String::new();
        let mut fixture = String::new();
        let mut timeout_secs = 120_u64;
        for part in body.split('|') {
            let Some((key, value)) = part.split_once('=') else {
                continue;
            };
            match key {
                "id" => id = value.to_string(),
                "instruction" => instruction = value.to_string(),
                "fixture" => fixture = value.to_string(),
                "timeout_secs" => {
                    timeout_secs = value.parse().map_err(|_| {
                        EvalError::InvalidTask(format!(
                            "invalid terminal-bench smoke timeout `{value}`"
                        ))
                    })?;
                }
                _ => {}
            }
        }
        if id.is_empty() || instruction.is_empty() {
            return Err(EvalError::InvalidTask(
                "terminal-bench smoke task requires id and instruction".to_string(),
            ));
        }
        if fixture != "local-rust" {
            return Err(EvalError::InvalidTask(format!(
                "unsupported terminal-bench smoke fixture `{fixture}`"
            )));
        }
        tasks.push(EvalTask {
            id,
            instruction,
            kind: EvalTaskKind::LocalRustFixture,
            timeout_secs,
        });
    }
    if tasks.is_empty() {
        return Err(EvalError::InvalidTask(
            "terminal-bench smoke lock contains no tasks".to_string(),
        ));
    }
    Ok(tasks)
}

fn terminal_bench_lock_value(content: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    content
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(&needle).map(str::to_string))
}

fn parse_swe_bench_verified_lock(content: &str) -> Result<Vec<SweBenchTask>, EvalError> {
    let mut tasks = Vec::new();
    for line in content.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || !line.starts_with("task=") {
            continue;
        }
        let body = line.trim_start_matches("task=");
        let mut instance_id = String::new();
        let mut repo = String::new();
        let mut base_commit = String::new();
        let mut version = String::new();
        let mut problem_statement = String::new();
        let mut fail_to_pass = Vec::new();
        let mut pass_to_pass = Vec::new();
        let mut timeout_secs = 120_u64;
        for part in body.split('|') {
            let Some((key, value)) = part.split_once('=') else {
                continue;
            };
            match key {
                "instance_id" => instance_id = value.to_string(),
                "repo" => repo = value.to_string(),
                "base_commit" => base_commit = value.to_string(),
                "version" => version = value.to_string(),
                "problem_statement" => problem_statement = value.to_string(),
                "fail_to_pass" => fail_to_pass = parse_csv(value),
                "pass_to_pass" => pass_to_pass = parse_csv(value),
                "timeout_secs" => {
                    timeout_secs = value.parse().map_err(|_| {
                        EvalError::InvalidTask(format!(
                            "invalid swe-bench verified timeout `{value}`"
                        ))
                    })?;
                }
                _ => {}
            }
        }
        if instance_id.is_empty() || repo.is_empty() || problem_statement.is_empty() {
            return Err(EvalError::InvalidTask(
                "swe-bench verified task requires instance_id, repo and problem_statement"
                    .to_string(),
            ));
        }
        tasks.push(SweBenchTask {
            instance_id,
            repo,
            base_commit,
            version,
            problem_statement,
            fail_to_pass,
            pass_to_pass,
            timeout_secs,
        });
    }
    if tasks.is_empty() {
        return Err(EvalError::InvalidTask(
            "swe-bench verified lock contains no tasks".to_string(),
        ));
    }
    Ok(tasks)
}

fn parse_csv(value: &str) -> Vec<String> {
    if value == "none" || value.is_empty() {
        return Vec::new();
    }
    value.split(',').map(str::to_string).collect()
}

async fn run_swe_bench_task(
    run: &EvalRun,
    task: &SweBenchTask,
) -> Result<SweBenchResult, EvalError> {
    let started = Instant::now();
    let workspace_path = run.path.join("workspace");
    if task.timeout_secs == 0 {
        return Ok(SweBenchResult {
            instance_id: task.instance_id.clone(),
            resolved: false,
            duration_ms: started.elapsed().as_millis(),
            command_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            session_id: None,
            events_path: None,
            patch_path: None,
            workspace_path,
            failure_kind: Some(SweBenchFailureKind::Timeout),
            failure_reason: Some("task timeout reached before execution".to_string()),
        });
    }
    let checkout_result = checkout_swe_bench_repo(run, task, &workspace_path);
    if let Err(error) = checkout_result {
        return Ok(SweBenchResult {
            instance_id: task.instance_id.clone(),
            resolved: false,
            duration_ms: started.elapsed().as_millis(),
            command_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            session_id: None,
            events_path: None,
            patch_path: None,
            workspace_path,
            failure_kind: Some(SweBenchFailureKind::EnvironmentFailure),
            failure_reason: Some(error.to_string()),
        });
    }
    init_workspace(&workspace_path)?;
    let issue_prompt = build_swe_bench_issue_prompt(task);
    fs::write(run.path.join("issue_prompt.md"), &issue_prompt)?;
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
    let agent_run = match runtime.run_task(&workspace_path, &issue_prompt).await {
        Ok(run) => run,
        Err(error) => {
            return Ok(SweBenchResult {
                instance_id: task.instance_id.clone(),
                resolved: false,
                duration_ms: started.elapsed().as_millis(),
                command_count: 0,
                input_tokens: 0,
                output_tokens: 0,
                session_id: None,
                events_path: None,
                patch_path: None,
                workspace_path,
                failure_kind: Some(SweBenchFailureKind::LocalizationFailure),
                failure_reason: Some(error.to_string()),
            });
        }
    };
    let events_path = workspace_path
        .join(".flash")
        .join("sessions")
        .join(&agent_run.session_id)
        .join("events.jsonl");
    let metrics = read_event_metrics(&events_path)?;
    let patch_path = run.path.join("model.patch");
    let patch_result = collect_patch(&workspace_path, &patch_path);
    if let Err(error) = patch_result {
        return Ok(SweBenchResult {
            instance_id: task.instance_id.clone(),
            resolved: false,
            duration_ms: started.elapsed().as_millis(),
            command_count: metrics.command_count,
            input_tokens: metrics.input_tokens,
            output_tokens: metrics.output_tokens,
            session_id: Some(agent_run.session_id),
            events_path: Some(events_path),
            patch_path: None,
            workspace_path,
            failure_kind: Some(SweBenchFailureKind::PatchFailure),
            failure_reason: Some(error.to_string()),
        });
    }
    let grader = run_swe_bench_grader(&workspace_path, run)?;
    let resolved = agent_run.outcome == Outcome::Succeeded
        && grader.status == ToolExitStatus::Success
        && fs::metadata(&patch_path)
            .map(|metadata| metadata.len() > 0)
            .unwrap_or(false);
    let failure_kind = if resolved {
        None
    } else if agent_run.outcome != Outcome::Succeeded {
        Some(SweBenchFailureKind::LocalizationFailure)
    } else if !patch_path.exists() {
        Some(SweBenchFailureKind::PatchFailure)
    } else {
        Some(SweBenchFailureKind::TestFailure)
    };
    let failure_reason = if resolved {
        None
    } else {
        Some(format!(
            "agent_outcome={}, grader_status={:?}",
            agent_run.outcome.as_str(),
            grader.status
        ))
    };
    Ok(SweBenchResult {
        instance_id: task.instance_id.clone(),
        resolved,
        duration_ms: started.elapsed().as_millis(),
        command_count: metrics.command_count,
        input_tokens: metrics.input_tokens,
        output_tokens: metrics.output_tokens,
        session_id: Some(agent_run.session_id),
        events_path: Some(events_path),
        patch_path: Some(patch_path),
        workspace_path,
        failure_kind,
        failure_reason,
    })
}

fn checkout_swe_bench_repo(
    run: &EvalRun,
    task: &SweBenchTask,
    workspace_path: &Path,
) -> Result<(), EvalError> {
    let cache_path = run.path.join("repo_cache").join(sanitize_id(&task.repo));
    setup_rust_fixture(&cache_path)?;
    copy_dir_recursive(&cache_path, workspace_path)?;
    fs::write(
        run.path.join("task_metadata.json"),
        format!(
            concat!(
                "{{",
                "\"instance_id\":\"{}\",",
                "\"repo\":\"{}\",",
                "\"base_commit\":\"{}\",",
                "\"version\":\"{}\"",
                "}}\n"
            ),
            json_string(&task.instance_id),
            json_string(&task.repo),
            json_string(&task.base_commit),
            json_string(&task.version)
        ),
    )?;
    Ok(())
}

fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<(), EvalError> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_dir_recursive(&source_path, &destination_path)?;
        } else {
            fs::copy(&source_path, &destination_path)?;
        }
    }
    Ok(())
}

fn build_swe_bench_issue_prompt(task: &SweBenchTask) -> String {
    format!(
        concat!(
            "SWE-bench Verified instance: {}\n",
            "Repository: {}\n",
            "Base commit: {}\n",
            "Version: {}\n\n",
            "Problem statement:\n{}\n\n",
            "Failing tests:\n{}\n\n",
            "Please fix failing tests and leave a minimal patch."
        ),
        task.instance_id,
        task.repo,
        task.base_commit,
        task.version,
        task.problem_statement,
        if task.fail_to_pass.is_empty() {
            "none".to_string()
        } else {
            task.fail_to_pass.join(",")
        }
    )
}

fn collect_patch(workspace_path: &Path, patch_path: &Path) -> Result<(), EvalError> {
    let output = Command::new("git")
        .args(["diff", "--"])
        .current_dir(workspace_path)
        .output()?;
    if !output.status.success() {
        return Err(EvalError::InvalidTask(format!(
            "failed to collect patch: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    fs::write(patch_path, &output.stdout)?;
    Ok(())
}

fn run_swe_bench_grader(root: &Path, run: &EvalRun) -> Result<flash_core::ToolOutput, EvalError> {
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

fn setup_rust_fixture(root: &Path) -> Result<(), EvalError> {
    fs::create_dir_all(root.join("src"))?;
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"flash_eval_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
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
            "\"command_count\":{},",
            "\"input_tokens\":{},",
            "\"output_tokens\":{},",
            "\"session_id\":\"{}\",",
            "\"events_path\":\"{}\",",
            "\"workspace_path\":\"{}\",",
            "\"failure_kind\":\"{}\",",
            "\"failure_reason\":\"{}\"",
            "}}\n"
        ),
        json_string(&result.task_id),
        result.passed,
        result.duration_ms,
        result.command_count,
        result.input_tokens,
        result.output_tokens,
        json_string(&session_id),
        json_string(&events_path),
        json_string(&result.workspace_path.display().to_string()),
        json_string(failure_kind),
        json_string(&failure_reason)
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
            "| command count | {} |\n",
            "| input tokens | {} |\n",
            "| output tokens | {} |\n",
            "| session id | {} |\n",
            "| events path | {} |\n",
            "| failure kind | {} |\n",
            "| failure reason | {} |\n"
        ),
        result.task_id,
        result.passed,
        result.duration_ms,
        result.command_count,
        result.input_tokens,
        result.output_tokens,
        session_id,
        events_path,
        failure_kind,
        failure_reason
    );
    fs::write(run.path.join("report.md"), content)?;
    Ok(())
}

fn write_terminal_bench_summary(run: &TerminalBenchRun) -> Result<(), EvalError> {
    let passed = run.results.iter().filter(|result| result.passed).count();
    let total = run.results.len();
    let mut json_tasks = String::new();
    for (index, result) in run.results.iter().enumerate() {
        if index > 0 {
            json_tasks.push(',');
        }
        json_tasks.push_str(&format!(
            concat!(
                "{{",
                "\"task_id\":\"{}\",",
                "\"passed\":{},",
                "\"duration_ms\":{},",
                "\"command_count\":{},",
                "\"input_tokens\":{},",
                "\"output_tokens\":{},",
                "\"failure_kind\":\"{}\",",
                "\"failure_reason\":\"{}\"",
                "}}"
            ),
            json_string(&result.task_id),
            result.passed,
            result.duration_ms,
            result.command_count,
            result.input_tokens,
            result.output_tokens,
            json_string(
                result
                    .failure_kind
                    .map(EvalFailureKind::as_str)
                    .unwrap_or("none")
            ),
            json_string(result.failure_reason.as_deref().unwrap_or("none"))
        ));
    }
    let result_json = format!(
        concat!(
            "{{",
            "\"benchmark\":\"terminal-bench\",",
            "\"subset\":\"{}\",",
            "\"lock_version\":\"{}\",",
            "\"passed\":{},",
            "\"total\":{},",
            "\"tasks\":[{}]",
            "}}\n"
        ),
        json_string(&run.subset),
        json_string(&run.lock_version),
        passed,
        total,
        json_tasks
    );
    fs::write(run.path.join("result.json"), result_json)?;

    let mut report = format!(
        concat!(
            "# Terminal-Bench Smoke Report\n\n",
            "| Field | Value |\n",
            "|---|---|\n",
            "| subset | {} |\n",
            "| lock version | {} |\n",
            "| passed | {}/{} |\n\n",
            "| Task | Pass | Duration ms | Commands | Tokens | Failure |\n",
            "|---|---:|---:|---:|---:|---|\n"
        ),
        run.subset, run.lock_version, passed, total
    );
    for result in &run.results {
        report.push_str(&format!(
            "| {} | {} | {} | {} | {}/{} | {} |\n",
            result.task_id,
            result.passed,
            result.duration_ms,
            result.command_count,
            result.input_tokens,
            result.output_tokens,
            result
                .failure_kind
                .map(EvalFailureKind::as_str)
                .unwrap_or("none")
        ));
    }
    fs::write(run.path.join("report.md"), report)?;
    Ok(())
}

fn write_swe_bench_task_result(run: &EvalRun, result: &SweBenchResult) -> Result<(), EvalError> {
    let events_path = result
        .events_path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let patch_path = result
        .patch_path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let session_id = result.session_id.clone().unwrap_or_default();
    let failure_kind = result
        .failure_kind
        .map(SweBenchFailureKind::as_str)
        .unwrap_or_default();
    let failure_reason = result.failure_reason.clone().unwrap_or_default();
    let content = format!(
        concat!(
            "{{",
            "\"instance_id\":\"{}\",",
            "\"resolved\":{},",
            "\"duration_ms\":{},",
            "\"command_count\":{},",
            "\"input_tokens\":{},",
            "\"output_tokens\":{},",
            "\"session_id\":\"{}\",",
            "\"events_path\":\"{}\",",
            "\"patch_path\":\"{}\",",
            "\"workspace_path\":\"{}\",",
            "\"failure_kind\":\"{}\",",
            "\"failure_reason\":\"{}\"",
            "}}\n"
        ),
        json_string(&result.instance_id),
        result.resolved,
        result.duration_ms,
        result.command_count,
        result.input_tokens,
        result.output_tokens,
        json_string(&session_id),
        json_string(&events_path),
        json_string(&patch_path),
        json_string(&result.workspace_path.display().to_string()),
        json_string(failure_kind),
        json_string(&failure_reason)
    );
    fs::write(run.path.join("result.json"), content)?;

    let content = format!(
        concat!(
            "# SWE-bench Verified Task Report\n\n",
            "| Field | Value |\n",
            "|---|---|\n",
            "| instance id | {} |\n",
            "| resolved | {} |\n",
            "| duration ms | {} |\n",
            "| command count | {} |\n",
            "| input tokens | {} |\n",
            "| output tokens | {} |\n",
            "| session id | {} |\n",
            "| events path | {} |\n",
            "| patch path | {} |\n",
            "| workspace path | {} |\n",
            "| failure kind | {} |\n",
            "| failure reason | {} |\n"
        ),
        result.instance_id,
        result.resolved,
        result.duration_ms,
        result.command_count,
        result.input_tokens,
        result.output_tokens,
        result.session_id.as_deref().unwrap_or("n/a"),
        if events_path.is_empty() {
            "n/a"
        } else {
            &events_path
        },
        if patch_path.is_empty() {
            "n/a"
        } else {
            &patch_path
        },
        result.workspace_path.display(),
        result
            .failure_kind
            .map(SweBenchFailureKind::as_str)
            .unwrap_or("none"),
        result.failure_reason.as_deref().unwrap_or("none")
    );
    fs::write(run.path.join("report.md"), content)?;
    Ok(())
}

fn write_swe_bench_summary(run: &SweBenchRun) -> Result<(), EvalError> {
    let summary = run.summary();
    let mut json_tasks = String::new();
    for (index, result) in run.results.iter().enumerate() {
        if index > 0 {
            json_tasks.push(',');
        }
        let events_path = result
            .events_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let patch_path = result
            .patch_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        json_tasks.push_str(&format!(
            concat!(
                "{{",
                "\"instance_id\":\"{}\",",
                "\"resolved\":{},",
                "\"duration_ms\":{},",
                "\"command_count\":{},",
                "\"input_tokens\":{},",
                "\"output_tokens\":{},",
                "\"session_id\":\"{}\",",
                "\"events_path\":\"{}\",",
                "\"patch_path\":\"{}\",",
                "\"failure_kind\":\"{}\",",
                "\"failure_reason\":\"{}\"",
                "}}"
            ),
            json_string(&result.instance_id),
            result.resolved,
            result.duration_ms,
            result.command_count,
            result.input_tokens,
            result.output_tokens,
            json_string(result.session_id.as_deref().unwrap_or_default()),
            json_string(&events_path),
            json_string(&patch_path),
            json_string(
                result
                    .failure_kind
                    .map(SweBenchFailureKind::as_str)
                    .unwrap_or("none")
            ),
            json_string(result.failure_reason.as_deref().unwrap_or("none"))
        ));
    }
    let result_json = format!(
        concat!(
            "{{",
            "\"benchmark\":\"swe-bench\",",
            "\"subset\":\"{}\",",
            "\"limit\":{},",
            "\"lock_version\":\"{}\",",
            "\"resolved\":{},",
            "\"unresolved\":{},",
            "\"environment_failures\":{},",
            "\"evaluated\":{},",
            "\"total\":{},",
            "\"tasks\":[{}]",
            "}}\n"
        ),
        json_string(&run.subset),
        run.limit,
        json_string(&run.lock_version),
        summary.resolved,
        summary.unresolved,
        summary.environment_failures,
        summary.evaluated,
        summary.total,
        json_tasks
    );
    fs::write(run.path.join("result.json"), result_json)?;

    let mut report = format!(
        concat!(
            "# SWE-bench Verified Smoke Report\n\n",
            "| Field | Value |\n",
            "|---|---|\n",
            "| subset | {} |\n",
            "| limit | {} |\n",
            "| lock version | {} |\n",
            "| resolved | {}/{} |\n",
            "| unresolved | {} |\n",
            "| environment failures | {} |\n",
            "| total | {} |\n\n",
            "| Instance | Resolved | Duration ms | Commands | Tokens | Patch | Failure |\n",
            "|---|---:|---:|---:|---:|---|---|\n"
        ),
        run.subset,
        run.limit,
        run.lock_version,
        summary.resolved,
        summary.evaluated,
        summary.unresolved,
        summary.environment_failures,
        summary.total
    );
    for result in &run.results {
        let patch_path = result
            .patch_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "n/a".to_string());
        report.push_str(&format!(
            "| {} | {} | {} | {} | {}/{} | {} | {} |\n",
            result.instance_id,
            result.resolved,
            result.duration_ms,
            result.command_count,
            result.input_tokens,
            result.output_tokens,
            patch_path,
            result
                .failure_kind
                .map(SweBenchFailureKind::as_str)
                .unwrap_or("none")
        ));
    }
    fs::write(run.path.join("report.md"), report)?;
    Ok(())
}

fn swe_bench_summary_counts(results: &[SweBenchResult]) -> SweBenchSummary {
    let total = results.len();
    let environment_failures = results
        .iter()
        .filter(|result| result.failure_kind == Some(SweBenchFailureKind::EnvironmentFailure))
        .count();
    let evaluated = total.saturating_sub(environment_failures);
    let resolved = results
        .iter()
        .filter(|result| result.resolved && result.failure_kind.is_none())
        .count();
    let unresolved = evaluated.saturating_sub(resolved);
    SweBenchSummary {
        total,
        evaluated,
        resolved,
        unresolved,
        environment_failures,
    }
}

fn regression_from_fixture(result: &EvalResult) -> RegressionBenchmarkResult {
    let failure = eval_failure(result.task_id.as_str(), result);
    RegressionBenchmarkResult {
        benchmark: "internal".to_string(),
        subset: "fixture".to_string(),
        lock_version: "internal-fixture-fix-rust-v1".to_string(),
        total: 1,
        passed: usize::from(result.passed),
        agent_failures: usize::from(matches!(
            result.failure_kind,
            Some(EvalFailureKind::AgentFailure)
        )),
        environment_failures: usize::from(matches!(
            result.failure_kind,
            Some(EvalFailureKind::EnvironmentFailure)
        )),
        benchmark_failures: usize::from(matches!(
            result.failure_kind,
            Some(EvalFailureKind::GraderFailure)
        )),
        report_path: result
            .workspace_path
            .parent()
            .map(|path| path.join("report.md"))
            .unwrap_or_else(|| result.workspace_path.join("report.md")),
        result_path: result
            .workspace_path
            .parent()
            .map(|path| path.join("result.json"))
            .unwrap_or_else(|| result.workspace_path.join("result.json")),
        replay_paths: result.events_path.iter().cloned().collect(),
        failures: failure.into_iter().collect(),
    }
}

fn regression_from_terminal_bench(run: &TerminalBenchRun) -> RegressionBenchmarkResult {
    let failures = run
        .results
        .iter()
        .filter_map(|result| eval_failure(result.task_id.as_str(), result))
        .collect::<Vec<_>>();
    RegressionBenchmarkResult {
        benchmark: "terminal-bench".to_string(),
        subset: run.subset.clone(),
        lock_version: run.lock_version.clone(),
        total: run.results.len(),
        passed: run.results.iter().filter(|result| result.passed).count(),
        agent_failures: failures
            .iter()
            .filter(|failure| failure.category == RegressionFailureCategory::Agent)
            .count(),
        environment_failures: failures
            .iter()
            .filter(|failure| failure.category == RegressionFailureCategory::Environment)
            .count(),
        benchmark_failures: failures
            .iter()
            .filter(|failure| failure.category == RegressionFailureCategory::Benchmark)
            .count(),
        report_path: run.path.join("report.md"),
        result_path: run.path.join("result.json"),
        replay_paths: run
            .results
            .iter()
            .filter_map(|result| result.events_path.clone())
            .collect(),
        failures,
    }
}

fn regression_from_swe_bench(run: &SweBenchRun) -> RegressionBenchmarkResult {
    let failures = run
        .results
        .iter()
        .filter_map(swe_failure)
        .collect::<Vec<_>>();
    RegressionBenchmarkResult {
        benchmark: "swe-bench".to_string(),
        subset: run.subset.clone(),
        lock_version: run.lock_version.clone(),
        total: run.results.len(),
        passed: run.results.iter().filter(|result| result.resolved).count(),
        agent_failures: failures
            .iter()
            .filter(|failure| failure.category == RegressionFailureCategory::Agent)
            .count(),
        environment_failures: failures
            .iter()
            .filter(|failure| failure.category == RegressionFailureCategory::Environment)
            .count(),
        benchmark_failures: failures
            .iter()
            .filter(|failure| failure.category == RegressionFailureCategory::Benchmark)
            .count(),
        report_path: run.path.join("report.md"),
        result_path: run.path.join("result.json"),
        replay_paths: run
            .results
            .iter()
            .filter_map(|result| result.events_path.clone())
            .collect(),
        failures,
    }
}

fn eval_failure(id: &str, result: &EvalResult) -> Option<RegressionFailure> {
    let kind = result.failure_kind?;
    let category = match kind {
        EvalFailureKind::AgentFailure => RegressionFailureCategory::Agent,
        EvalFailureKind::EnvironmentFailure => RegressionFailureCategory::Environment,
        EvalFailureKind::GraderFailure => RegressionFailureCategory::Benchmark,
    };
    Some(RegressionFailure {
        id: id.to_string(),
        category,
        reason: result
            .failure_reason
            .clone()
            .unwrap_or_else(|| kind.as_str().to_string()),
        replay_path: result.events_path.clone(),
    })
}

fn swe_failure(result: &SweBenchResult) -> Option<RegressionFailure> {
    let kind = result.failure_kind?;
    let category = match kind {
        SweBenchFailureKind::LocalizationFailure | SweBenchFailureKind::PatchFailure => {
            RegressionFailureCategory::Agent
        }
        SweBenchFailureKind::EnvironmentFailure | SweBenchFailureKind::Timeout => {
            RegressionFailureCategory::Environment
        }
        SweBenchFailureKind::TestFailure => RegressionFailureCategory::Benchmark,
    };
    Some(RegressionFailure {
        id: result.instance_id.clone(),
        category,
        reason: result
            .failure_reason
            .clone()
            .unwrap_or_else(|| kind.as_str().to_string()),
        replay_path: result.events_path.clone(),
    })
}

fn pass_rate_bps(passed: usize, total: usize) -> u64 {
    if total == 0 {
        return 0;
    }
    ((passed as u64) * 10_000) / (total as u64)
}

fn format_rate(bps: u64) -> String {
    format!("{}.{:02}%", bps / 100, bps % 100)
}

fn regression_failure_ids(benchmarks: &[RegressionBenchmarkResult]) -> Vec<String> {
    let mut ids = Vec::new();
    for benchmark in benchmarks {
        for failure in &benchmark.failures {
            ids.push(format!("{}/{}", benchmark.benchmark, failure.id));
        }
    }
    ids
}

fn new_failure_ids(current: &[String], previous: &[String]) -> Vec<String> {
    current
        .iter()
        .filter(|failure| !previous.contains(failure))
        .cloned()
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RegressionSnapshot {
    pass_rate_bps: u64,
    failures: Vec<String>,
}

fn latest_previous_regression(
    root: &Path,
    current_path: &Path,
) -> Result<Option<RegressionSnapshot>, EvalError> {
    let evals_path = root.join(".flash").join("evals");
    if !evals_path.exists() {
        return Ok(None);
    }
    let mut candidates = Vec::new();
    for entry in fs::read_dir(evals_path)? {
        let entry = entry?;
        let path = entry.path();
        if path == current_path {
            continue;
        }
        let result_path = path.join("regression_result.json");
        if !result_path.exists() {
            continue;
        }
        let modified = entry
            .metadata()?
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH);
        candidates.push((modified, result_path));
    }
    candidates.sort_by_key(|(modified, _)| *modified);
    let Some((_, result_path)) = candidates.pop() else {
        return Ok(None);
    };
    parse_regression_snapshot(&fs::read_to_string(result_path)?)
}

fn parse_regression_snapshot(content: &str) -> Result<Option<RegressionSnapshot>, EvalError> {
    let Some(rate) = json_number_field(content, "pass_rate_bps") else {
        return Ok(None);
    };
    let pass_rate_bps = rate
        .parse()
        .map_err(|_| EvalError::InvalidTask("invalid regression pass_rate_bps".to_string()))?;
    Ok(Some(RegressionSnapshot {
        pass_rate_bps,
        failures: parse_json_string_array(content, "failures"),
    }))
}

fn parse_json_string_array(content: &str, key: &str) -> Vec<String> {
    let needle = format!("\"{key}\":[");
    let Some(start) = content.find(&needle).map(|index| index + needle.len()) else {
        return Vec::new();
    };
    let rest = &content[start..];
    let Some(end) = rest.find(']') else {
        return Vec::new();
    };
    rest[..end]
        .split(',')
        .filter_map(|part| {
            let trimmed = part.trim();
            if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
                Some(trimmed[1..trimmed.len() - 1].replace("\\\"", "\""))
            } else {
                None
            }
        })
        .collect()
}

fn write_regression_summary(run: &RegressionRun) -> Result<(), EvalError> {
    fs::write(&run.result_path, regression_result_json(run))?;
    fs::write(&run.trend_path, regression_trend_json(run))?;
    fs::write(&run.report_path, regression_report_markdown(run))?;
    Ok(())
}

fn regression_result_json(run: &RegressionRun) -> String {
    let failures = json_string_array(&regression_failure_ids(&run.benchmarks));
    let new_failures = json_string_array(&run.new_failures);
    let previous_rate = run
        .previous_pass_rate_bps
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".to_string());
    let mut benchmarks = String::new();
    for (index, benchmark) in run.benchmarks.iter().enumerate() {
        if index > 0 {
            benchmarks.push(',');
        }
        benchmarks.push_str(&format!(
            concat!(
                "{{",
                "\"benchmark\":\"{}\",",
                "\"subset\":\"{}\",",
                "\"lock_version\":\"{}\",",
                "\"passed\":{},",
                "\"total\":{},",
                "\"agent_failures\":{},",
                "\"environment_failures\":{},",
                "\"benchmark_failures\":{},",
                "\"report_path\":\"{}\",",
                "\"result_path\":\"{}\"",
                "}}"
            ),
            json_string(&benchmark.benchmark),
            json_string(&benchmark.subset),
            json_string(&benchmark.lock_version),
            benchmark.passed,
            benchmark.total,
            benchmark.agent_failures,
            benchmark.environment_failures,
            benchmark.benchmark_failures,
            json_string(&benchmark.report_path.display().to_string()),
            json_string(&benchmark.result_path.display().to_string())
        ));
    }
    format!(
        concat!(
            "{{",
            "\"kind\":\"regression\",",
            "\"passed\":{},",
            "\"failed\":{},",
            "\"total\":{},",
            "\"pass_rate_bps\":{},",
            "\"previous_pass_rate_bps\":{},",
            "\"failures\":{},",
            "\"new_failures\":{},",
            "\"benchmarks\":[{}]",
            "}}\n"
        ),
        run.passed,
        run.failed,
        run.total,
        run.pass_rate_bps,
        previous_rate,
        failures,
        new_failures,
        benchmarks
    )
}

fn regression_trend_json(run: &RegressionRun) -> String {
    let previous_rate = run
        .previous_pass_rate_bps
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".to_string());
    format!(
        concat!(
            "{{",
            "\"kind\":\"regression_trend\",",
            "\"run_id\":\"{}\",",
            "\"pass_rate_bps\":{},",
            "\"previous_pass_rate_bps\":{},",
            "\"delta_bps\":{},",
            "\"new_failures\":{}",
            "}}\n"
        ),
        json_string(&run.run_id),
        run.pass_rate_bps,
        previous_rate,
        run.previous_pass_rate_bps
            .map(|previous| run.pass_rate_bps as i64 - previous as i64)
            .unwrap_or(0),
        json_string_array(&run.new_failures)
    )
}

fn regression_report_markdown(run: &RegressionRun) -> String {
    let previous_rate = run
        .previous_pass_rate_bps
        .map(format_rate)
        .unwrap_or_else(|| "n/a".to_string());
    let delta = run
        .previous_pass_rate_bps
        .map(|previous| format!("{} bps", run.pass_rate_bps as i64 - previous as i64))
        .unwrap_or_else(|| "n/a".to_string());
    let mut report = format!(
        concat!(
            "# Flash Regression Report\n\n",
            "| Field | Value |\n",
            "|---|---|\n",
            "| passed | {}/{} |\n",
            "| pass rate | {} |\n",
            "| previous pass rate | {} |\n",
            "| delta | {} |\n",
            "| new failures | {} |\n\n",
            "| Benchmark | Subset | Lock version | Passed | Agent failures | Environment failures | Benchmark failures | Report |\n",
            "|---|---|---|---:|---:|---:|---:|---|\n"
        ),
        run.passed,
        run.total,
        format_rate(run.pass_rate_bps),
        previous_rate,
        delta,
        run.new_failures.len()
    );
    for benchmark in &run.benchmarks {
        report.push_str(&format!(
            "| {} | {} | {} | {}/{} | {} | {} | {} | {} |\n",
            benchmark.benchmark,
            benchmark.subset,
            benchmark.lock_version,
            benchmark.passed,
            benchmark.total,
            benchmark.agent_failures,
            benchmark.environment_failures,
            benchmark.benchmark_failures,
            benchmark.report_path.display()
        ));
    }
    report.push_str("\n## Replay Links\n\n");
    for benchmark in &run.benchmarks {
        for replay_path in &benchmark.replay_paths {
            report.push_str(&format!(
                "- {} {}: {}\n",
                benchmark.benchmark,
                benchmark.subset,
                replay_path.display()
            ));
        }
    }
    report.push_str("\n## New Failures\n\n");
    if run.new_failures.is_empty() {
        report.push_str("- none\n");
    } else {
        for failure in &run.new_failures {
            report.push_str(&format!("- {failure}\n"));
        }
    }
    report.push_str("\n## Failures\n\n");
    if run.failed == 0 {
        report.push_str("- none\n");
    } else {
        for benchmark in &run.benchmarks {
            for failure in &benchmark.failures {
                let replay = failure
                    .replay_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "n/a".to_string());
                report.push_str(&format!(
                    "- {}/{} [{}]: {} replay={}\n",
                    benchmark.benchmark,
                    failure.id,
                    failure.category.as_str(),
                    failure.reason,
                    replay
                ));
            }
        }
    }
    report
}

fn json_string_array(values: &[String]) -> String {
    let mut content = String::from("[");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            content.push(',');
        }
        content.push('"');
        content.push_str(&json_string(value));
        content.push('"');
    }
    content.push(']');
    content
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EventMetrics {
    command_count: u64,
    input_tokens: u64,
    output_tokens: u64,
}

fn read_event_metrics(events_path: &Path) -> Result<EventMetrics, EvalError> {
    let content = fs::read_to_string(events_path)?;
    let mut metrics = EventMetrics {
        command_count: 0,
        input_tokens: 0,
        output_tokens: 0,
    };
    for line in content.lines() {
        if line.contains("\"type\":\"tool_started\"") {
            metrics.command_count += 1;
        }
        if line.contains("\"type\":\"usage_recorded\"") {
            metrics.input_tokens += json_number_field(line, "input_tokens")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            metrics.output_tokens += json_number_field(line, "output_tokens")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
        }
    }
    Ok(metrics)
}

fn json_number_field(content: &str, key: &str) -> Option<String> {
    json_value_field(content, key).and_then(|value| value.as_u64().map(|number| number.to_string()))
}

fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn json_string(input: &str) -> String {
    let encoded = serde_json::to_string(input).unwrap_or_else(|_| "\"\"".to_string());
    encoded
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or_default()
        .to_string()
}

fn json_value_field(content: &str, key: &str) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    value
        .get(key)
        .cloned()
        .or_else(|| value.get("event").and_then(|event| event.get(key)).cloned())
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

    #[tokio::test]
    async fn run_fixture_eval_should_write_result_report_and_events() {
        let root = temp_dir("fixture_eval");
        fs::create_dir_all(&root).unwrap();

        let result = run_fixture_eval(&root, local_fixture_task("fix-rust").unwrap())
            .await
            .unwrap();

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

    #[tokio::test]
    async fn terminal_bench_smoke_should_write_summary_and_task_results() {
        let root = temp_dir("terminal_bench_smoke");
        fs::create_dir_all(&root).unwrap();

        let run = run_terminal_bench_smoke(&root).await.unwrap();

        assert_eq!(run.subset, "smoke");
        assert_eq!(run.results.len(), 1);
        assert!(run.path.join("terminal_bench_smoke.lock").exists());
        assert!(run.path.join("result.json").exists());
        assert!(run.path.join("report.md").exists());
        assert!(run.results[0].events_path.as_ref().unwrap().exists());
    }

    #[test]
    fn terminal_bench_smoke_lock_should_have_fixed_task_subset() {
        let tasks = terminal_bench_smoke_tasks().unwrap();

        assert_eq!(tasks[0].id, "terminal-bench-smoke/local-rust-fix");
    }

    #[test]
    fn swe_bench_verified_tasks_should_load_fixed_task_metadata() {
        let tasks = swe_bench_verified_tasks().unwrap();

        assert_eq!(tasks.len(), 10);
        assert_eq!(tasks[0].instance_id, "flash-code__local-rust-1");
        assert_eq!(tasks[9].instance_id, "flash-code__local-rust-10");
        assert_eq!(tasks[0].repo, "flash-code/local-rust-fixture");
        assert_eq!(tasks[0].fail_to_pass, vec!["answer_should_be_42"]);
    }

    #[tokio::test]
    async fn run_swe_bench_verified_should_write_patch_events_and_grader_logs() {
        let root = temp_dir("swe_bench_verified");
        fs::create_dir_all(&root).unwrap();

        let run = run_swe_bench_verified(&root, 1).await.unwrap();

        assert_eq!(run.subset, "verified");
        assert_eq!(run.results.len(), 1);
        assert!(run.results[0].resolved);
        assert!(run.path.join("swe_bench_verified_smoke.lock").exists());
        assert!(run.path.join("result.json").exists());
        assert!(run.path.join("report.md").exists());
        let task_dir = run.path.join("tasks").join("flash-code__local-rust-1");
        assert!(task_dir.join("task_metadata.json").exists());
        assert!(task_dir.join("issue_prompt.md").exists());
        assert!(task_dir.join("grader.stdout.txt").exists());
        assert!(task_dir.join("grader.stderr.txt").exists());
        assert!(run.results[0].events_path.as_ref().unwrap().exists());
        let patch_path = run.results[0].patch_path.as_ref().unwrap();
        assert!(patch_path.exists());
        assert!(fs::read_to_string(patch_path).unwrap().contains("+    42"));
    }

    #[tokio::test]
    async fn run_swe_bench_verified_should_run_fixed_10_task_subset() {
        let root = temp_dir("swe_bench_verified_10");
        fs::create_dir_all(&root).unwrap();

        let run = run_swe_bench_verified(&root, 10).await.unwrap();
        let summary = run.summary();

        assert_eq!(run.results.len(), 10);
        assert_eq!(summary.resolved, 10);
        assert_eq!(summary.evaluated, 10);
        assert_eq!(summary.environment_failures, 0);
        assert!(run.results.iter().all(|result| result.session_id.is_some()));
        assert!(run
            .results
            .iter()
            .all(|result| result.patch_path.as_ref().is_some_and(|path| path.exists())));
    }

    #[tokio::test]
    async fn run_regression_should_write_report_trend_and_replay_links() {
        let root = temp_dir("regression");
        fs::create_dir_all(&root).unwrap();

        let run = run_regression_with_swe_limit(&root, 1).await.unwrap();

        assert_eq!(run.total, 3);
        assert_eq!(run.passed, 3);
        assert!(run.report_path.exists());
        assert!(run.result_path.exists());
        assert!(run.trend_path.exists());
        let report = fs::read_to_string(&run.report_path).unwrap();
        assert!(report.contains("Replay Links"));
        assert!(report.contains("internal-fixture-fix-rust-v1"));
        assert!(report.contains("terminal-bench"));
        assert!(report.contains("swe-bench"));
        assert!(fs::read_to_string(&run.result_path)
            .unwrap()
            .contains("\"kind\":\"regression\""));
    }

    #[tokio::test]
    async fn run_regression_should_compare_against_previous_result() {
        let root = temp_dir("regression_previous");
        fs::create_dir_all(&root).unwrap();

        let first = run_regression_with_swe_limit(&root, 1).await.unwrap();
        let second = run_regression_with_swe_limit(&root, 1).await.unwrap();

        assert_eq!(second.previous_pass_rate_bps, Some(first.pass_rate_bps));
        assert!(second.new_failures.is_empty());
        assert!(fs::read_to_string(&second.trend_path)
            .unwrap()
            .contains("\"previous_pass_rate_bps\":10000"));
    }

    #[test]
    fn swe_bench_summary_should_exclude_environment_failures_from_evaluated_count() {
        let results = vec![
            swe_result("resolved", true, None),
            swe_result("unresolved", false, Some(SweBenchFailureKind::TestFailure)),
            swe_result(
                "environment",
                false,
                Some(SweBenchFailureKind::EnvironmentFailure),
            ),
        ];

        let summary = swe_bench_summary_counts(&results);

        assert_eq!(
            summary,
            SweBenchSummary {
                total: 3,
                evaluated: 2,
                resolved: 1,
                unresolved: 1,
                environment_failures: 1,
            }
        );
    }

    #[tokio::test]
    async fn swe_bench_task_should_record_timeout_before_execution() {
        let root = temp_dir("swe_bench_timeout");
        let run = EvalRun {
            id: "timeout".to_string(),
            path: root,
        };
        fs::create_dir_all(&run.path).unwrap();
        let mut task = swe_bench_verified_tasks().unwrap().remove(0);
        task.timeout_secs = 0;

        let result = run_swe_bench_task(&run, &task).await.unwrap();

        assert!(!result.resolved);
        assert_eq!(result.failure_kind, Some(SweBenchFailureKind::Timeout));
        assert!(!result.workspace_path.exists());
    }

    #[test]
    fn swe_bench_failure_kind_should_have_stable_json_names() {
        assert_eq!(
            SweBenchFailureKind::LocalizationFailure.as_str(),
            "localization_failure"
        );
        assert_eq!(SweBenchFailureKind::PatchFailure.as_str(), "patch_failure");
        assert_eq!(SweBenchFailureKind::TestFailure.as_str(), "test_failure");
        assert_eq!(
            SweBenchFailureKind::EnvironmentFailure.as_str(),
            "environment_failure"
        );
        assert_eq!(SweBenchFailureKind::Timeout.as_str(), "timeout");
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

    fn swe_result(
        instance_id: &str,
        resolved: bool,
        failure_kind: Option<SweBenchFailureKind>,
    ) -> SweBenchResult {
        SweBenchResult {
            instance_id: instance_id.to_string(),
            resolved,
            duration_ms: 0,
            command_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            session_id: None,
            events_path: None,
            patch_path: None,
            workspace_path: PathBuf::from("workspace"),
            failure_kind,
            failure_reason: None,
        }
    }
}
