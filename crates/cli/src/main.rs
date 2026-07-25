use std::cell::RefCell;
use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};

use flash_agent::{AgentOptions, AgentRuntime, ApprovalController, ApprovalRequest, SmokeProvider};
use flash_core::{
    discover_workspace_root, init_workspace, replay_events, Config, ConfigOverrides, Event,
    PermissionPolicy,
};

fn main() {
    if let Err(error) = run(env::args().skip(1).collect()) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run(args: Vec<String>) -> Result<(), CliError> {
    match args.first().map(String::as_str) {
        None => tui(),
        Some("--version" | "-V") => {
            println!("flash {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("init") => init(),
        Some("doctor") => doctor(),
        Some("run") => run_task(&args[1..]),
        Some("eval") => eval(&args[1..]),
        Some("replay") => replay(&args[1..]),
        Some("resume") => resume(&args[1..]),
        Some("tui") => tui(),
        Some("help" | "--help" | "-h") => {
            print_help();
            Ok(())
        }
        Some(command) => Err(CliError::Usage(format!("unknown command `{command}`"))),
    }
}

fn tui() -> Result<(), CliError> {
    let mut runner = CliTaskRunner;
    flash_tui::run_current_workspace(&mut runner).map_err(CliError::Tui)
}

struct CliTaskRunner;

impl flash_tui::TaskRunner for CliTaskRunner {
    fn permission_mode(&mut self, workspace_root: &Path) -> String {
        load_config(workspace_root, &ConfigOverrides::default())
            .map(|config| format!("{:?}", config.approval_mode))
            .unwrap_or_else(|_| "unknown".to_string())
    }

    fn run_task(
        &mut self,
        workspace_root: &Path,
        task: &str,
        controller: &mut dyn flash_tui::RunController,
    ) -> Result<flash_tui::TuiRun, String> {
        let config = load_config(workspace_root, &ConfigOverrides::default())
            .map_err(|error| error.to_string())?;
        let registry = flash_tools::builtin_registry().map_err(|error| error.to_string())?;
        let mut runtime = AgentRuntime::new(
            SmokeProvider::new(),
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
        let run = runtime
            .run_task_with_controls(
                workspace_root,
                task,
                &mut runtime_observer,
                || controller_cell.borrow_mut().should_cancel(),
                &mut runtime_approval,
            )
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
    if env::var(&config.deepseek_api_key_env).is_ok() {
        println!("api_key_env: {} present", config.deepseek_api_key_env);
    } else {
        println!("api_key_env: {} missing", config.deepseek_api_key_env);
    }
    Ok(())
}

fn run_task(args: &[String]) -> Result<(), CliError> {
    let Some(task) = args.first() else {
        return Err(CliError::Usage(
            "usage: flash run \"fix the failing tests\"".to_string(),
        ));
    };
    let root = discover_workspace_root(None)?;
    let config = load_config(&root, &ConfigOverrides::default())?;
    let registry = flash_tools::builtin_registry().map_err(CliError::ToolRegistry)?;
    let mut runtime = AgentRuntime::new(
        SmokeProvider::new(),
        registry,
        AgentOptions {
            model: config.deepseek_model,
            max_turns: config.max_turns,
            permission_policy: PermissionPolicy::new(config.approval_mode),
            max_output_bytes: config.shell_max_output_bytes,
            max_prompt_bytes: 200_000,
        },
    );
    let run = runtime.run_task(&root, task)?;
    println!("session: {}", run.session_id);
    println!("outcome: {}", run.outcome.as_str());
    Ok(())
}

fn eval(args: &[String]) -> Result<(), CliError> {
    match args.first().map(String::as_str) {
        Some("fixture") => eval_fixture(&args[1..]),
        Some(command) => Err(CliError::Usage(format!("unknown eval command `{command}`"))),
        None => Err(CliError::Usage(
            "usage: flash eval fixture --task fix-rust".to_string(),
        )),
    }
}

fn eval_fixture(args: &[String]) -> Result<(), CliError> {
    let task_id = parse_task_arg(args)?;
    let root = discover_workspace_root(None)?;
    init_workspace(&root)?;
    let task = flash_eval::local_fixture_task(&task_id)?;
    let result = flash_eval::run_fixture_eval(&root, task)?;
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

fn parse_task_arg(args: &[String]) -> Result<String, CliError> {
    let Some(flag_index) = args.iter().position(|arg| arg == "--task") else {
        return Err(CliError::Usage(
            "usage: flash eval fixture --task fix-rust".to_string(),
        ));
    };
    args.get(flag_index + 1)
        .cloned()
        .ok_or_else(|| CliError::Usage("usage: flash eval fixture --task fix-rust".to_string()))
}

fn replay(args: &[String]) -> Result<(), CliError> {
    let Some(path) = args.first() else {
        return Err(CliError::Usage(
            "usage: flash replay .flash/sessions/session_xxx/events.jsonl".to_string(),
        ));
    };
    for line in replay_events(Path::new(path))? {
        println!("{line}");
    }
    Ok(())
}

fn resume(args: &[String]) -> Result<(), CliError> {
    let Some(session_id) = args.first() else {
        return Err(CliError::Usage(
            "usage: flash resume session_xxx".to_string(),
        ));
    };
    let root = discover_workspace_root(None)?;
    let session = flash_core::storage::load_session(&root, session_id)?;
    println!("resumed {}", session.id);
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

fn print_help() {
    println!(concat!(
        "flash 0.1\n\n",
        "Usage:\n",
        "  flash init\n",
        "  flash tui\n",
        "  flash doctor\n",
        "  flash run \"<task>\"\n",
        "  flash eval fixture --task fix-rust\n",
        "  flash replay <events.jsonl>\n",
        "  flash resume <session_id>\n\n",
        "Running `flash` without a subcommand enters the TUI.\n"
    ));
}

#[derive(Debug)]
enum CliError {
    Config(flash_core::config::ConfigError),
    Agent(flash_agent::AgentError),
    Eval(flash_eval::EvalError),
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

impl From<flash_core::WorkspaceError> for CliError {
    fn from(error: flash_core::WorkspaceError) -> Self {
        Self::Workspace(error)
    }
}
