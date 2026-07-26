use std::cell::RefCell;
use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use clap::{Args, Parser, Subcommand};
use flash_agent::{AgentOptions, AgentRuntime, ApprovalController, ApprovalRequest, SmokeProvider};
use flash_core::{
    discover_workspace_root, init_workspace, recover_session, replay_events, Config,
    ConfigOverrides, Event, PermissionPolicy,
};
use flash_deepseek::DeepSeekProvider;
use flash_provider::{ChatProvider, ChatRequest, ProviderError, ProviderEvent};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = run(Cli::parse()).await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(name = "flash", version, about = "Flash Code workspace agent")]
struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
#[command(rename_all = "kebab-case")]
enum CliCommand {
    Init,
    Tui,
    Doctor,
    Run(RunArgs),
    Continue(ContinueArgs),
    Eval {
        #[command(subcommand)]
        command: EvalCommand,
    },
    Replay(ReplayArgs),
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
struct RunArgs {
    task: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
struct ContinueArgs {
    session_id: String,
    instruction: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
#[command(rename_all = "kebab-case")]
enum EvalCommand {
    Fixture(EvalFixtureArgs),
    TerminalBench(EvalTerminalBenchArgs),
    SweBench(EvalSweBenchArgs),
    Regression,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
struct EvalFixtureArgs {
    #[arg(long)]
    task: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
struct EvalTerminalBenchArgs {
    #[arg(long)]
    subset: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
struct EvalSweBenchArgs {
    #[arg(long)]
    subset: String,
    #[arg(long, default_value_t = 1)]
    limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
struct ReplayArgs {
    session_id: String,
}

async fn run(cli: Cli) -> Result<(), CliError> {
    match cli.command {
        None | Some(CliCommand::Tui) => tui().await,
        Some(CliCommand::Init) => init(),
        Some(CliCommand::Doctor) => doctor(),
        Some(CliCommand::Run(args)) => run_task(&args.task).await,
        Some(CliCommand::Continue(args)) => {
            continue_task(&args.session_id, &args.instruction).await
        }
        Some(CliCommand::Eval { command }) => eval(command).await,
        Some(CliCommand::Replay(args)) => replay(&args.session_id),
    }
}

async fn tui() -> Result<(), CliError> {
    let mut runner = CliTaskRunner;
    flash_tui::run_current_workspace(&mut runner)
        .await
        .map_err(CliError::Tui)
}

struct CliTaskRunner;

enum CliProvider {
    DeepSeek(DeepSeekProvider),
    Smoke(SmokeProvider),
}

#[async_trait(?Send)]
impl ChatProvider for CliProvider {
    async fn chat(
        &mut self,
        request: ChatRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        match self {
            Self::DeepSeek(provider) => provider.chat(request, events).await,
            Self::Smoke(provider) => provider.chat(request, events).await,
        }
    }
}

#[async_trait(?Send)]
impl flash_tui::TaskRunner for CliTaskRunner {
    fn permission_mode(&mut self, workspace_root: &Path) -> String {
        load_config(workspace_root, &ConfigOverrides::default())
            .map(|config| format!("{:?}", config.approval_mode))
            .unwrap_or_else(|_| "unknown".to_string())
    }

    async fn run_task(
        &mut self,
        workspace_root: &Path,
        task: &str,
        controller: &mut dyn flash_tui::RunController,
    ) -> Result<flash_tui::TuiRun, String> {
        let config = load_config(workspace_root, &ConfigOverrides::default())
            .map_err(|error| error.to_string())?;
        let registry = flash_tools::builtin_registry_with_options(
            config.shell_timeout_secs,
            config.shell_max_output_bytes,
            config.allow_network,
        )
        .map_err(|error| error.to_string())?;
        let provider = provider_from_config(&config).map_err(|error| error.to_string())?;
        let mut runtime = AgentRuntime::new(
            provider,
            registry,
            AgentOptions {
                model: config.deepseek_model,
                max_turns: config.max_turns,
                permission_policy: PermissionPolicy::new(config.approval_mode),
                max_output_bytes: config.shell_max_output_bytes,
                max_prompt_bytes: 200_000,
            },
        );
        let controller_cell = RefCell::new(controller);
        let mut runtime_observer = |event: &Event| controller_cell.borrow_mut().on_event(event);
        let mut runtime_approval = CliApprovalController {
            controller: &controller_cell,
        };
        let cancellation = controller_cell.borrow().cancellation_token();
        let run = runtime
            .run_task_with_cancellation(
                workspace_root,
                task,
                &mut runtime_observer,
                cancellation,
                &mut runtime_approval,
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(flash_tui::TuiRun {
            session_id: run.session_id,
            outcome: run.outcome.as_str().to_string(),
        })
    }
}

struct CliApprovalController<'cell, 'controller> {
    controller: &'cell RefCell<&'controller mut dyn flash_tui::RunController>,
}

impl ApprovalController for CliApprovalController<'_, '_> {
    fn approve(&mut self, request: &ApprovalRequest) -> bool {
        self.controller
            .borrow_mut()
            .approve(&flash_tui::ApprovalPrompt {
                call_id: request.call_id.clone(),
                name: request.name.clone(),
                input: request.input.clone(),
                risk: format!("{:?}", request.risk),
            })
    }
}

fn init() -> Result<(), CliError> {
    let root = discover_workspace_root(None)?;
    let workspace = init_workspace(&root)?;
    println!("initialized {}", workspace.root.display());
    Ok(())
}

fn doctor() -> Result<(), CliError> {
    let root = discover_workspace_root(None)?;
    init_workspace(&root)?;
    let config = load_config(&root, &ConfigOverrides::default())?;
    println!("workspace_root: {}", root.display());
    println!("provider: {}", config.provider_default);
    println!("model: {}", config.deepseek_model);
    if config.allow_network {
        println!("bash_network_policy: allowed after permission policy");
    } else if cfg!(target_os = "macos") {
        println!("bash_network_policy: denied by offline allowlist and macOS sandbox");
    } else {
        println!(
            "bash_network_policy: degraded to offline allowlist; unknown commands are denied because no OS network sandbox is available"
        );
    }
    if env::var(&config.deepseek_api_key_env).is_ok() {
        println!("api_key_env: {} present", config.deepseek_api_key_env);
    } else {
        println!("api_key_env: {} missing", config.deepseek_api_key_env);
    }
    Ok(())
}

async fn run_task(task: &str) -> Result<(), CliError> {
    let root = discover_workspace_root(None)?;
    let config = load_config(&root, &ConfigOverrides::default())?;
    let mut runtime = configured_runtime(config)?;
    let run = runtime.run_task(&root, task).await?;
    println!("session: {}", run.session_id);
    println!("outcome: {}", run.outcome.as_str());
    Ok(())
}

async fn continue_task(session_id: &str, instruction: &str) -> Result<(), CliError> {
    let root = discover_workspace_root(None)?;
    recover_session(&root, session_id)?;
    let config = load_config(&root, &ConfigOverrides::default())?;
    let mut runtime = configured_runtime(config)?;
    let run = runtime
        .continue_task(&root, session_id, instruction)
        .await?;
    println!("session: {}", run.session_id);
    println!("parent_session: {session_id}");
    println!("outcome: {}", run.outcome.as_str());
    Ok(())
}

fn configured_runtime(config: Config) -> Result<AgentRuntime<CliProvider>, CliError> {
    let registry = flash_tools::builtin_registry_with_options(
        config.shell_timeout_secs,
        config.shell_max_output_bytes,
        config.allow_network,
    )
    .map_err(CliError::ToolRegistry)?;
    let provider = provider_from_config(&config)?;
    Ok(AgentRuntime::new(
        provider,
        registry,
        AgentOptions {
            model: config.deepseek_model,
            max_turns: config.max_turns,
            permission_policy: PermissionPolicy::new(config.approval_mode),
            max_output_bytes: config.shell_max_output_bytes,
            max_prompt_bytes: 200_000,
        },
    ))
}

fn provider_from_config(config: &Config) -> Result<CliProvider, CliError> {
    match config.provider_default.as_str() {
        "deepseek" => Ok(CliProvider::DeepSeek(DeepSeekProvider::from_env(
            &config.deepseek_base_url,
            &config.deepseek_api_key_env,
        )?)),
        "smoke" => Ok(CliProvider::Smoke(SmokeProvider::new())),
        provider => Err(CliError::Usage(format!(
            "unsupported provider `{provider}`; expected `deepseek` or `smoke`"
        ))),
    }
}

async fn eval(command: EvalCommand) -> Result<(), CliError> {
    match command {
        EvalCommand::Fixture(args) => eval_fixture(&args.task).await,
        EvalCommand::TerminalBench(args) => eval_terminal_bench(&args.subset).await,
        EvalCommand::SweBench(args) => eval_swe_bench(&args.subset, args.limit).await,
        EvalCommand::Regression => eval_regression().await,
    }
}

async fn eval_fixture(task_id: &str) -> Result<(), CliError> {
    let root = discover_workspace_root(None)?;
    init_workspace(&root)?;
    let task = flash_eval::local_fixture_task(task_id)?;
    let result = flash_eval::run_fixture_eval(&root, task).await?;
    println!("task: {}", result.task_id);
    println!("passed: {}", result.passed);
    if let Some(session_id) = &result.session_id {
        println!("session: {session_id}");
    }
    if let Some(events_path) = &result.events_path {
        println!("events: {}", events_path.display());
    }
    println!(
        "eval_run: {}",
        result
            .workspace_path
            .parent()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| result.workspace_path.display().to_string())
    );
    if let Some(kind) = result.failure_kind {
        println!("failure_kind: {}", kind.as_str());
    }
    Ok(())
}

async fn eval_terminal_bench(subset: &str) -> Result<(), CliError> {
    if subset != "smoke" {
        return Err(CliError::Usage(
            "usage: flash eval terminal-bench --subset smoke".to_string(),
        ));
    }
    let root = discover_workspace_root(None)?;
    init_workspace(&root)?;
    let run = flash_eval::run_terminal_bench_smoke(&root).await?;
    let passed = run.results.iter().filter(|result| result.passed).count();
    println!("benchmark: terminal-bench");
    println!("subset: {}", run.subset);
    println!("lock_version: {}", run.lock_version);
    println!("passed: {passed}/{}", run.results.len());
    println!("eval_run: {}", run.path.display());
    println!("report: {}", run.path.join("report.md").display());
    println!("result: {}", run.path.join("result.json").display());
    for result in run.results {
        println!(
            "task: {} pass={} commands={} tokens={}/{}",
            result.task_id,
            result.passed,
            result.command_count,
            result.input_tokens,
            result.output_tokens
        );
        if let Some(events_path) = result.events_path {
            println!("events: {}", events_path.display());
        }
    }
    Ok(())
}

async fn eval_swe_bench(subset: &str, limit: usize) -> Result<(), CliError> {
    let usage = "usage: flash eval swe-bench --subset verified --limit 10";
    if subset != "verified" {
        return Err(CliError::Usage(usage.to_string()));
    }
    let root = discover_workspace_root(None)?;
    init_workspace(&root)?;
    let run = flash_eval::run_swe_bench_verified(&root, limit).await?;
    let summary = run.summary();
    println!("benchmark: swe-bench");
    println!("subset: {}", run.subset);
    println!("lock_version: {}", run.lock_version);
    println!("resolved: {}/{}", summary.resolved, summary.evaluated);
    println!("unresolved: {}", summary.unresolved);
    println!("environment_failures: {}", summary.environment_failures);
    println!("eval_run: {}", run.path.display());
    println!("report: {}", run.path.join("report.md").display());
    println!("result: {}", run.path.join("result.json").display());
    for result in run.results {
        println!(
            "task: {} resolved={} commands={} tokens={}/{}",
            result.instance_id,
            result.resolved,
            result.command_count,
            result.input_tokens,
            result.output_tokens
        );
        if let Some(session_id) = result.session_id {
            println!("session: {session_id}");
        }
        if let Some(events_path) = result.events_path {
            println!("events: {}", events_path.display());
        }
        if let Some(patch_path) = result.patch_path {
            println!("patch: {}", patch_path.display());
        }
        if let Some(kind) = result.failure_kind {
            println!("failure_kind: {}", kind.as_str());
        }
    }
    Ok(())
}

async fn eval_regression() -> Result<(), CliError> {
    let root = discover_workspace_root(None)?;
    init_workspace(&root)?;
    let run = flash_eval::run_regression(&root).await?;
    println!("benchmark: regression");
    println!("passed: {}/{}", run.passed, run.total);
    println!("pass_rate_bps: {}", run.pass_rate_bps);
    match run.previous_pass_rate_bps {
        Some(previous) => println!("previous_pass_rate_bps: {previous}"),
        None => println!("previous_pass_rate_bps: none"),
    }
    println!("new_failures: {}", run.new_failures.len());
    println!("eval_run: {}", run.path.display());
    println!("report: {}", run.report_path.display());
    println!("result: {}", run.result_path.display());
    println!("trend: {}", run.trend_path.display());
    for benchmark in run.benchmarks {
        println!(
            "benchmark_result: {} subset={} pass={}/{} agent_failures={} environment_failures={} benchmark_failures={}",
            benchmark.benchmark,
            benchmark.subset,
            benchmark.passed,
            benchmark.total,
            benchmark.agent_failures,
            benchmark.environment_failures,
            benchmark.benchmark_failures
        );
        println!("benchmark_report: {}", benchmark.report_path.display());
    }
    Ok(())
}

fn replay(session_id: &str) -> Result<(), CliError> {
    let root = discover_workspace_root(None)?;
    let session = recover_session(&root, session_id)?;
    for line in replay_events(&session.path.join("events.jsonl"))? {
        println!("{line}");
    }
    Ok(())
}

fn load_config(root: &Path, overrides: &ConfigOverrides) -> Result<Config, CliError> {
    let user_config = user_config_path();
    let workspace_config = root.join(".flash").join("config.toml");
    Config::load(
        user_config.as_deref(),
        Some(&workspace_config),
        &env_map(),
        overrides,
    )
    .map_err(CliError::Config)
}

fn user_config_path() -> Option<PathBuf> {
    env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("flash-code")
            .join("config.toml")
    })
}

fn env_map() -> BTreeMap<String, String> {
    env::vars().collect()
}

#[derive(Debug)]
enum CliError {
    Config(flash_core::config::ConfigError),
    Agent(flash_agent::AgentError),
    Eval(flash_eval::EvalError),
    Provider(ProviderError),
    Storage(flash_core::storage::StorageError),
    ToolRegistry(flash_core::tools::ToolRegistryError),
    Tui(flash_tui::TuiError),
    Workspace(flash_core::WorkspaceError),
    Usage(String),
}

impl std::fmt::Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(error) => write!(formatter, "{error}"),
            Self::Agent(error) => write!(formatter, "{error}"),
            Self::Eval(error) => write!(formatter, "{error}"),
            Self::Provider(error) => write!(formatter, "{error}"),
            Self::Storage(error) => write!(formatter, "{error}"),
            Self::ToolRegistry(error) => write!(formatter, "{error}"),
            Self::Tui(error) => write!(formatter, "{error}"),
            Self::Workspace(error) => write!(formatter, "{error}"),
            Self::Usage(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for CliError {}

impl From<flash_core::config::ConfigError> for CliError {
    fn from(error: flash_core::config::ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<flash_core::storage::StorageError> for CliError {
    fn from(error: flash_core::storage::StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<flash_agent::AgentError> for CliError {
    fn from(error: flash_agent::AgentError) -> Self {
        Self::Agent(error)
    }
}

impl From<flash_eval::EvalError> for CliError {
    fn from(error: flash_eval::EvalError) -> Self {
        Self::Eval(error)
    }
}

impl From<ProviderError> for CliError {
    fn from(error: ProviderError) -> Self {
        Self::Provider(error)
    }
}

impl From<flash_core::WorkspaceError> for CliError {
    fn from(error: flash_core::WorkspaceError) -> Self {
        Self::Workspace(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn cli_should_default_to_tui_when_no_subcommand_is_present() {
        let cli = Cli::try_parse_from(["flash"]).unwrap();

        assert_eq!(cli.command, None);
    }

    #[test]
    fn cli_should_parse_run_task() {
        let cli = Cli::try_parse_from(["flash", "run", "fix tests"]).unwrap();

        assert_eq!(
            cli.command,
            Some(CliCommand::Run(RunArgs {
                task: "fix tests".to_string()
            }))
        );
    }

    #[test]
    fn cli_should_parse_eval_terminal_bench() {
        let cli =
            Cli::try_parse_from(["flash", "eval", "terminal-bench", "--subset", "smoke"]).unwrap();

        assert_eq!(
            cli.command,
            Some(CliCommand::Eval {
                command: EvalCommand::TerminalBench(EvalTerminalBenchArgs {
                    subset: "smoke".to_string()
                })
            })
        );
    }

    #[test]
    fn cli_should_parse_replay_and_reject_legacy_resume() {
        let cli = Cli::try_parse_from(["flash", "replay", "session_123"]).unwrap();

        assert_eq!(
            cli.command,
            Some(CliCommand::Replay(ReplayArgs {
                session_id: "session_123".to_string()
            }))
        );
        let error = Cli::try_parse_from(["flash", "resume", "session_123"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn cli_should_parse_continue_with_explicit_instruction() {
        let cli = Cli::try_parse_from([
            "flash",
            "continue",
            "session_123",
            "finish the remaining tests",
        ])
        .unwrap();

        assert_eq!(
            cli.command,
            Some(CliCommand::Continue(ContinueArgs {
                session_id: "session_123".to_string(),
                instruction: "finish the remaining tests".to_string(),
            }))
        );
    }

    #[test]
    fn cli_should_reject_unknown_command_before_runtime() {
        let error = Cli::try_parse_from(["flash", "unknown"]).unwrap_err();

        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn cli_should_reject_missing_run_task_before_runtime() {
        let error = Cli::try_parse_from(["flash", "run"]).unwrap_err();

        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }
}
