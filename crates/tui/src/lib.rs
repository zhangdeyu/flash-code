use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use flash_core::storage::load_session;
use flash_core::{discover_workspace_root, init_workspace, Event};

const DEFAULT_WIDTH: usize = 100;
const DEFAULT_HEIGHT: usize = 32;

pub trait TaskRunner {
    fn permission_mode(&mut self, workspace_root: &Path) -> String;

    fn run_task(
        &mut self,
        workspace_root: &Path,
        task: &str,
        controller: &mut dyn RunController,
    ) -> Result<TuiRun, String>;
}

pub trait RunController {
    fn on_event(&mut self, event: &Event);

    fn approve(&mut self, prompt: &ApprovalPrompt) -> bool;

    fn should_cancel(&mut self) -> bool;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalPrompt {
    pub call_id: String,
    pub name: String,
    pub input: String,
    pub risk: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiRun {
    pub session_id: String,
    pub outcome: String,
}

pub fn run_current_workspace(runner: &mut impl TaskRunner) -> Result<(), TuiError> {
    let root = discover_workspace_root(None)?;
    init_workspace(&root)?;
    let mut state = AppState::load(&root)?;
    state.permission_mode = runner.permission_mode(&root);
    let mut stdout = io::stdout();

    if let Some(session_id) = std::env::var_os("FLASH_TUI_RESUME") {
        state.resume_session(&root, &session_id.to_string_lossy());
    }

    if let Some(task) = std::env::var_os("FLASH_TUI_TASK") {
        run_task_for_state(
            &root,
            &mut state,
            runner,
            &task.to_string_lossy(),
            &mut stdout,
        )?;
    }

    if should_render_once() {
        stdout.write_all(render_to_string(&state, DEFAULT_WIDTH, DEFAULT_HEIGHT).as_bytes())?;
        stdout.flush()?;
        return Ok(());
    }

    let _terminal = TerminalGuard::enter()?;
    render_frame(&mut stdout, &state)?;
    input_loop(&root, &mut stdout, &mut state, runner)?;
    Ok(())
}

fn should_render_once() -> bool {
    std::env::var_os("FLASH_TUI_ONCE").is_some()
        || !io::stdin().is_terminal()
        || !io::stdout().is_terminal()
}

fn render_frame(stdout: &mut impl Write, state: &AppState) -> Result<(), TuiError> {
    write!(stdout, "\x1b[2J\x1b[H")?;
    stdout.write_all(render_to_string(state, DEFAULT_WIDTH, DEFAULT_HEIGHT).as_bytes())?;
    stdout.flush()?;
    Ok(())
}

fn input_loop(
    workspace_root: &Path,
    stdout: &mut impl Write,
    state: &mut AppState,
    runner: &mut impl TaskRunner,
) -> Result<(), TuiError> {
    let mut stdin = io::stdin();
    let mut buffer = [0_u8; 1];
    loop {
        let read = stdin.read(&mut buffer)?;
        if read == 0 || matches!(buffer[0], 3 | 27) {
            state.cancel();
            render_frame(stdout, state)?;
            break;
        }
        match buffer[0] {
            b'\r' | b'\n' => {
                if !state.input.is_empty() {
                    let task = state.input.clone();
                    state.input.clear();
                    if let Some(session_id) = task.strip_prefix("resume ") {
                        state.resume_session(workspace_root, session_id.trim());
                        render_frame(stdout, state)?;
                    } else {
                        run_task_for_state(workspace_root, state, runner, &task, stdout)?;
                    }
                }
            }
            8 | 127 => {
                state.input.pop();
                render_frame(stdout, state)?;
            }
            b'q' | b'Q' if state.input.is_empty() => break,
            byte if byte.is_ascii_graphic() || byte == b' ' => {
                state.input.push(byte as char);
                render_frame(stdout, state)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn run_task_for_state(
    workspace_root: &Path,
    state: &mut AppState,
    runner: &mut impl TaskRunner,
    task: &str,
    stdout: &mut impl Write,
) -> Result<(), TuiError> {
    state.start_task(task);
    render_frame(stdout, state)?;
    let mut controller = UiRunController {
        state,
        stdout,
        render_error: None,
    };
    let run = runner
        .run_task(workspace_root, task, &mut controller)
        .map_err(TuiError::Runner)?;
    if let Some(error) = controller.render_error {
        return Err(error);
    }
    controller.state.finish_task(&run);
    render_frame(controller.stdout, controller.state)?;
    controller.state.reload_sessions(workspace_root)?;
    Ok(())
}

struct UiRunController<'a, W> {
    state: &'a mut AppState,
    stdout: &'a mut W,
    render_error: Option<TuiError>,
}

impl<W> RunController for UiRunController<'_, W>
where
    W: Write,
{
    fn on_event(&mut self, event: &Event) {
        self.state.push_event(event);
        if let Err(error) = render_frame(self.stdout, self.state) {
            self.render_error = Some(error);
        }
    }

    fn approve(&mut self, prompt: &ApprovalPrompt) -> bool {
        self.state.set_pending_approval(prompt);
        if let Err(error) = render_frame(self.stdout, self.state) {
            self.render_error = Some(error);
            return false;
        }
        approval_from_env().unwrap_or_else(read_approval_from_stdin)
    }

    fn should_cancel(&mut self) -> bool {
        std::env::var_os("FLASH_TUI_CANCEL_AFTER_START").is_some()
    }
}

fn approval_from_env() -> Option<bool> {
    std::env::var_os("FLASH_TUI_APPROVE").map(|value| {
        matches!(
            value.to_string_lossy().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "y" | "approve"
        )
    })
}

fn read_approval_from_stdin() -> bool {
    if !io::stdin().is_terminal() {
        return false;
    }
    let mut stdin = io::stdin();
    let mut buffer = [0_u8; 1];
    loop {
        let Ok(read) = stdin.read(&mut buffer) else {
            return false;
        };
        if read == 0 {
            return false;
        }
        match buffer[0] {
            b'y' | b'Y' => return true,
            b'n' | b'N' | 3 | 27 => return false,
            _ => {}
        }
    }
}

struct TerminalGuard {
    restore_raw_mode: bool,
}

impl TerminalGuard {
    fn enter() -> Result<Self, TuiError> {
        let restore_raw_mode = set_raw_mode();
        let mut stdout = io::stdout();
        write!(stdout, "\x1b[?1049h\x1b[?25l")?;
        stdout.flush()?;
        Ok(Self { restore_raw_mode })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.restore_raw_mode {
            let _status = Command::new("stty").arg("sane").status();
        }
        let mut stdout = io::stdout();
        let _result = write!(stdout, "\x1b[?25h\x1b[?1049l");
        let _result = stdout.flush();
    }
}

fn set_raw_mode() -> bool {
    Command::new("stty")
        .args(["raw", "-echo"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppState {
    workspace_root: PathBuf,
    sessions: Vec<SessionSummary>,
    transcript: Vec<TranscriptLine>,
    input: String,
    status: RunStatus,
    current_session_id: Option<String>,
    pending_approval: Option<ApprovalPrompt>,
    permission_mode: String,
}

impl AppState {
    pub fn load(workspace_root: &Path) -> Result<Self, TuiError> {
        let mut sessions = load_sessions(workspace_root)?;
        sessions.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| right.session_id.cmp(&left.session_id))
        });
        let transcript = sessions
            .first()
            .map(|session| load_transcript(&session.events_path))
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            workspace_root: workspace_root.to_path_buf(),
            sessions,
            transcript,
            input: String::new(),
            status: RunStatus::Idle,
            current_session_id: None,
            pending_approval: None,
            permission_mode: "unknown".to_string(),
        })
    }

    pub fn from_events(workspace_root: PathBuf, events: &[&str]) -> Self {
        Self {
            workspace_root,
            sessions: Vec::new(),
            transcript: events
                .iter()
                .filter_map(|line| parse_event_line(line))
                .collect(),
            input: String::new(),
            status: RunStatus::Idle,
            current_session_id: None,
            pending_approval: None,
            permission_mode: "unknown".to_string(),
        }
    }

    fn start_task(&mut self, task: &str) {
        self.status = RunStatus::Running;
        self.current_session_id = None;
        self.pending_approval = None;
        self.transcript.clear();
        self.transcript.push(TranscriptLine {
            kind: TranscriptKind::Input,
            text: task.to_string(),
        });
    }

    fn finish_task(&mut self, run: &TuiRun) {
        self.current_session_id = Some(run.session_id.clone());
        self.status = match run.outcome.as_str() {
            "succeeded" => RunStatus::Succeeded,
            "cancelled" => RunStatus::Cancelled,
            _ => RunStatus::Failed,
        };
        self.pending_approval = None;
    }

    fn cancel(&mut self) {
        self.status = RunStatus::Cancelled;
        self.transcript.push(TranscriptLine {
            kind: TranscriptKind::Session,
            text: "cancel requested".to_string(),
        });
    }

    fn push_event(&mut self, event: &Event) {
        if let Some(line) = event_to_transcript(event) {
            self.transcript.push(line);
        }
        if matches!(event, Event::ApprovalResolved { .. }) {
            self.pending_approval = None;
        }
    }

    fn set_pending_approval(&mut self, prompt: &ApprovalPrompt) {
        self.pending_approval = Some(prompt.clone());
        self.transcript.push(TranscriptLine {
            kind: TranscriptKind::Approval,
            text: format!(
                "pending {} {} risk={}",
                prompt.name, prompt.call_id, prompt.risk
            ),
        });
    }

    fn reload_sessions(&mut self, workspace_root: &Path) -> Result<(), TuiError> {
        let mut sessions = load_sessions(workspace_root)?;
        sessions.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| right.session_id.cmp(&left.session_id))
        });
        self.sessions = sessions;
        Ok(())
    }

    fn resume_session(&mut self, workspace_root: &Path, session_id: &str) {
        match load_session(workspace_root, session_id) {
            Ok(session) => match load_transcript(&session.path.join("events.jsonl")) {
                Ok(transcript) => {
                    self.current_session_id = Some(session.id);
                    self.status = RunStatus::Idle;
                    self.pending_approval = None;
                    self.transcript = transcript;
                }
                Err(error) => self.replace_with_error(format!("resume failed: {error}")),
            },
            Err(error) => self.replace_with_error(format!("resume failed: {error}")),
        }
    }

    fn push_error(&mut self, message: String) {
        self.status = RunStatus::Failed;
        self.transcript.push(TranscriptLine {
            kind: TranscriptKind::Error,
            text: message,
        });
    }

    fn replace_with_error(&mut self, message: String) {
        self.transcript.clear();
        self.push_error(message);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunStatus {
    Idle,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl RunStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionSummary {
    session_id: String,
    updated_at: String,
    status: String,
    events_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TranscriptLine {
    kind: TranscriptKind,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscriptKind {
    User,
    Assistant,
    Reasoning,
    Tool,
    Approval,
    Error,
    Input,
    Session,
}

pub fn render_to_string(state: &AppState, width: usize, height: usize) -> String {
    let width = width.max(40);
    let height = height.max(12);
    let content_width = width.saturating_sub(4);
    let mut lines = Vec::new();

    lines.push(horizontal(width));
    lines.push(row(width, "Flash Code"));
    lines.push(row(
        width,
        &format!("Workspace: {}", state.workspace_root.display()),
    ));
    lines.push(row(
        width,
        &format!(
            "Status: {}{}",
            state.status.as_str(),
            state
                .current_session_id
                .as_ref()
                .map(|session_id| format!("  Session: {session_id}"))
                .unwrap_or_default()
        ),
    ));
    lines.push(row(
        width,
        &format!("Permission: {}", state.permission_mode),
    ));
    if let Some(prompt) = &state.pending_approval {
        lines.push(row(
            width,
            &format!(
                "Approval: {} {} risk={}  y approve / n reject",
                prompt.name, prompt.call_id, prompt.risk
            ),
        ));
    }
    lines.push(horizontal(width));
    lines.push(row(width, "Sessions"));

    if state.sessions.is_empty() {
        lines.push(row(width, "  No sessions in this workspace yet."));
    } else {
        for session in state.sessions.iter().take(5) {
            lines.push(row(
                width,
                &format!(
                    "  {}  {}  {}",
                    session.session_id, session.status, session.updated_at
                ),
            ));
        }
    }

    lines.push(horizontal(width));
    lines.push(row(width, "Transcript"));

    if state.transcript.is_empty() {
        lines.push(row(width, "  No events to display."));
    } else {
        for entry in state
            .transcript
            .iter()
            .flat_map(|entry| render_entry(entry, content_width))
        {
            lines.push(row(width, &entry));
            if lines.len() + 1 >= height {
                break;
            }
        }
    }

    lines.push(horizontal(width));
    lines.push(row(width, &format!("Input: {}", state.input)));
    lines.push(row(
        width,
        "Enter submits, q quits, Esc/Ctrl-C cancels before submit.",
    ));

    while lines.len() + 1 < height {
        lines.push(row(width, ""));
    }
    lines.push(horizontal(width));
    lines.join("\n") + "\n"
}

fn render_entry(entry: &TranscriptLine, width: usize) -> Vec<String> {
    let label = match entry.kind {
        TranscriptKind::User => "user",
        TranscriptKind::Assistant => "assistant",
        TranscriptKind::Reasoning => "reasoning",
        TranscriptKind::Tool => "tool",
        TranscriptKind::Approval => "approval",
        TranscriptKind::Error => "error",
        TranscriptKind::Input => "input",
        TranscriptKind::Session => "session",
    };
    wrap(&format!("{label}: {}", entry.text), width)
}

fn horizontal(width: usize) -> String {
    format!("+{}+", "-".repeat(width.saturating_sub(2)))
}

fn row(width: usize, text: &str) -> String {
    let content_width = width.saturating_sub(4);
    let clipped = clip(text, content_width);
    format!("| {clipped:<content_width$} |")
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let separator = usize::from(!current.is_empty());
        if current.len() + separator + word.len() > width && !current.is_empty() {
            lines.push(current);
            current = String::new();
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
        .into_iter()
        .flat_map(|line| split_long_line(&line, width))
        .collect()
}

fn split_long_line(text: &str, width: usize) -> Vec<String> {
    if text.len() <= width {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if current.len() + ch.len_utf8() > width {
            lines.push(current);
            current = String::new();
        }
        current.push(ch);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn clip(text: &str, width: usize) -> String {
    text.chars()
        .scan(0, |used, ch| {
            let len = ch.len_utf8();
            if *used + len > width {
                None
            } else {
                *used += len;
                Some(ch)
            }
        })
        .collect()
}

fn load_sessions(workspace_root: &Path) -> Result<Vec<SessionSummary>, TuiError> {
    let sessions_dir = workspace_root.join(".flash").join("sessions");
    if !sessions_dir.exists() {
        return Ok(Vec::new());
    }
    let mut sessions = Vec::new();
    for entry in fs::read_dir(&sessions_dir)? {
        let entry = entry?;
        let session_json = entry.path().join("session.json");
        if !session_json.exists() {
            continue;
        }
        let content = fs::read_to_string(&session_json)?;
        let Some(session_id) = find_json_string(&content, "session_id") else {
            continue;
        };
        sessions.push(SessionSummary {
            session_id,
            updated_at: find_json_string(&content, "updated_at").unwrap_or_default(),
            status: find_json_string(&content, "status").unwrap_or_else(|| "unknown".to_string()),
            events_path: entry.path().join("events.jsonl"),
        });
    }
    Ok(sessions)
}

fn load_transcript(events_path: &Path) -> Result<Vec<TranscriptLine>, TuiError> {
    let content = fs::read_to_string(events_path)?;
    Ok(content.lines().filter_map(parse_event_line).collect())
}

fn parse_event_line(line: &str) -> Option<TranscriptLine> {
    let event_type = find_json_string(line, "type")?;
    match event_type.as_str() {
        "session_started" => Some(TranscriptLine {
            kind: TranscriptKind::Session,
            text: format!(
                "started {}",
                find_json_string(line, "session_id").unwrap_or_default()
            ),
        }),
        "user_message_appended" => Some(TranscriptLine {
            kind: TranscriptKind::User,
            text: format!(
                "message {} appended",
                find_json_string(line, "message_id").unwrap_or_default()
            ),
        }),
        "model_request_started" => Some(TranscriptLine {
            kind: TranscriptKind::Session,
            text: format!(
                "model request {}",
                find_json_string(line, "model").unwrap_or_default()
            ),
        }),
        "reasoning_delta" => Some(TranscriptLine {
            kind: TranscriptKind::Reasoning,
            text: find_json_string(line, "text").unwrap_or_default(),
        }),
        "assistant_delta" => Some(TranscriptLine {
            kind: TranscriptKind::Assistant,
            text: find_json_string(line, "text").unwrap_or_default(),
        }),
        "tool_call_requested" => Some(TranscriptLine {
            kind: TranscriptKind::Tool,
            text: format!(
                "requested {} {}",
                find_json_string(line, "name").unwrap_or_default(),
                find_json_string(line, "call_id").unwrap_or_default()
            ),
        }),
        "approval_required" => Some(TranscriptLine {
            kind: TranscriptKind::Approval,
            text: format!(
                "required for {}",
                find_json_string(line, "call_id").unwrap_or_default()
            ),
        }),
        "approval_resolved" => Some(TranscriptLine {
            kind: TranscriptKind::Approval,
            text: format!(
                "{} approved={}",
                find_json_string(line, "call_id").unwrap_or_default(),
                find_json_bool(line, "approved").unwrap_or(false)
            ),
        }),
        "tool_started" => Some(TranscriptLine {
            kind: TranscriptKind::Tool,
            text: format!(
                "started {} {}",
                find_json_string(line, "name").unwrap_or_default(),
                find_json_string(line, "call_id").unwrap_or_default()
            ),
        }),
        "tool_output_delta" => Some(TranscriptLine {
            kind: TranscriptKind::Tool,
            text: format!(
                "{}: {}",
                find_json_string(line, "stream").unwrap_or_default(),
                find_json_string(line, "text").unwrap_or_default()
            ),
        }),
        "tool_finished" => Some(TranscriptLine {
            kind: TranscriptKind::Tool,
            text: format!(
                "finished {} {}",
                find_json_string(line, "call_id").unwrap_or_default(),
                find_json_string(line, "status").unwrap_or_default()
            ),
        }),
        "usage_recorded" => Some(TranscriptLine {
            kind: TranscriptKind::Session,
            text: format!(
                "usage input={} output={}",
                find_json_number(line, "input_tokens").unwrap_or_default(),
                find_json_number(line, "output_tokens").unwrap_or_default()
            ),
        }),
        "error" => Some(TranscriptLine {
            kind: TranscriptKind::Error,
            text: find_json_string(line, "message").unwrap_or_default(),
        }),
        "session_finished" => Some(TranscriptLine {
            kind: TranscriptKind::Session,
            text: format!(
                "finished {}",
                find_json_string(line, "outcome").unwrap_or_default()
            ),
        }),
        _ => None,
    }
}

fn event_to_transcript(event: &Event) -> Option<TranscriptLine> {
    match event {
        Event::SessionStarted { session_id } => Some(TranscriptLine {
            kind: TranscriptKind::Session,
            text: format!("started {session_id}"),
        }),
        Event::UserMessageAppended { message_id } => Some(TranscriptLine {
            kind: TranscriptKind::User,
            text: format!("message {message_id} appended"),
        }),
        Event::ModelRequestStarted { model, .. } => Some(TranscriptLine {
            kind: TranscriptKind::Session,
            text: format!("model request {model}"),
        }),
        Event::ReasoningDelta { text } => Some(TranscriptLine {
            kind: TranscriptKind::Reasoning,
            text: text.clone(),
        }),
        Event::AssistantDelta { text } => Some(TranscriptLine {
            kind: TranscriptKind::Assistant,
            text: text.clone(),
        }),
        Event::AssistantMessageCompleted { message_id } => Some(TranscriptLine {
            kind: TranscriptKind::Assistant,
            text: format!("message {message_id} completed"),
        }),
        Event::ToolCallRequested { call_id, name } => Some(TranscriptLine {
            kind: TranscriptKind::Tool,
            text: format!("requested {name} {call_id}"),
        }),
        Event::ApprovalRequired { call_id } => Some(TranscriptLine {
            kind: TranscriptKind::Approval,
            text: format!("required for {call_id}"),
        }),
        Event::ApprovalResolved { call_id, approved } => Some(TranscriptLine {
            kind: TranscriptKind::Approval,
            text: format!("{call_id} approved={approved}"),
        }),
        Event::ToolStarted { call_id, name } => Some(TranscriptLine {
            kind: TranscriptKind::Tool,
            text: format!("started {name} {call_id}"),
        }),
        Event::ToolOutputDelta { stream, text, .. } => Some(TranscriptLine {
            kind: TranscriptKind::Tool,
            text: format!("{stream}: {text}"),
        }),
        Event::ToolFinished { call_id, status } => Some(TranscriptLine {
            kind: TranscriptKind::Tool,
            text: format!("finished {} {}", call_id, status.as_str()),
        }),
        Event::UsageRecorded {
            input_tokens,
            output_tokens,
        } => Some(TranscriptLine {
            kind: TranscriptKind::Session,
            text: format!("usage input={input_tokens} output={output_tokens}"),
        }),
        Event::Error { message } => Some(TranscriptLine {
            kind: TranscriptKind::Error,
            text: message.clone(),
        }),
        Event::SessionFinished { outcome } => Some(TranscriptLine {
            kind: TranscriptKind::Session,
            text: format!("finished {}", outcome.as_str()),
        }),
    }
}

fn find_json_string(content: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = content.find(&needle)? + needle.len();
    let rest = &content[start..];
    let end = json_string_end(rest)?;
    Some(unescape_json_string(&rest[..end]))
}

fn json_string_end(value: &str) -> Option<usize> {
    let mut escaped = false;
    for (index, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => return Some(index),
            _ => {}
        }
    }
    None
}

fn find_json_number(content: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":");
    let start = content.find(&needle)? + needle.len();
    let rest = &content[start..];
    let end = rest
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

fn find_json_bool(content: &str, key: &str) -> Option<bool> {
    let needle = format!("\"{key}\":");
    let start = content.find(&needle)? + needle.len();
    let rest = &content[start..];
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

fn unescape_json_string(value: &str) -> String {
    value
        .replace("\\n", "\n")
        .replace("\\r", "\r")
        .replace("\\t", "\t")
        .replace("\\\"", "\"")
        .replace("\\\\", "\\")
}

#[derive(Debug)]
pub enum TuiError {
    Io(io::Error),
    CoreWorkspace(flash_core::WorkspaceError),
    CoreStorage(flash_core::storage::StorageError),
    Runner(String),
}

impl std::fmt::Display for TuiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "tui io error: {error}"),
            Self::CoreWorkspace(error) => write!(formatter, "{error}"),
            Self::CoreStorage(error) => write!(formatter, "{error}"),
            Self::Runner(message) => write!(formatter, "tui runner error: {message}"),
        }
    }
}

impl std::error::Error for TuiError {}

impl From<io::Error> for TuiError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<flash_core::WorkspaceError> for TuiError {
    fn from(error: flash_core::WorkspaceError) -> Self {
        Self::CoreWorkspace(error)
    }
}

impl From<flash_core::storage::StorageError> for TuiError {
    fn from(error: flash_core::storage::StorageError) -> Self {
        Self::CoreStorage(error)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use flash_core::{append_event, create_session, Event, Outcome, ToolResultStatus};

    use super::*;

    #[test]
    fn render_to_string_should_include_empty_state() {
        let state = empty_state();

        let output = render_to_string(&state, 60, 12);

        assert!(output.contains("No sessions in this workspace yet."));
    }

    #[test]
    fn parse_event_line_should_render_reasoning_delta() {
        let line = r#"{"event":{"type":"reasoning_delta","text":"inspect files"}}"#;

        let entry = parse_event_line(line).unwrap();

        assert_eq!(entry.kind, TranscriptKind::Reasoning);
    }

    #[test]
    fn parse_event_line_should_keep_escaped_quotes_in_text() {
        let line = r#"{"event":{"type":"assistant_delta","text":"say \"hello\" now"}}"#;

        let entry = parse_event_line(line).unwrap();

        assert_eq!(entry.text, "say \"hello\" now");
    }

    #[test]
    fn app_state_load_should_scan_sessions_without_index() {
        let root = temp_dir("scan_sessions");
        fs::create_dir_all(&root).unwrap();
        let session = create_session(&root).unwrap();
        append_event(
            &session,
            Event::AssistantDelta {
                text: "hello from replay".to_string(),
            },
        )
        .unwrap();

        let state = AppState::load(&root).unwrap();

        assert_eq!(state.sessions.len(), 1);
    }

    #[test]
    fn resume_session_should_load_transcript_for_current_workspace() {
        let root = temp_dir("resume_current");
        fs::create_dir_all(&root).unwrap();
        let session = create_session(&root).unwrap();
        append_event(
            &session,
            Event::AssistantDelta {
                text: "hello resume".to_string(),
            },
        )
        .unwrap();
        let mut state = AppState::load(&root).unwrap();

        state.resume_session(&root, &session.id);

        assert_eq!(state.current_session_id, Some(session.id));
    }

    #[test]
    fn resume_session_should_render_workspace_mismatch_error() {
        let root = temp_dir("resume_a");
        let other = temp_dir("resume_b");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&other).unwrap();
        let session = create_session(&root).unwrap();
        let other_session = other.join(".flash/sessions").join(&session.id);
        fs::create_dir_all(&other_session).unwrap();
        fs::copy(
            session.path.join("session.json"),
            other_session.join("session.json"),
        )
        .unwrap();
        fs::write(other_session.join("events.jsonl"), "").unwrap();
        let mut state = AppState::load(&other).unwrap();

        state.resume_session(&other, &session.id);

        assert_eq!(state.status, RunStatus::Failed);
        assert!(state.transcript[0].text.contains("session belongs to"));
    }

    #[test]
    fn render_to_string_should_include_tool_approval_and_error() {
        let events = [
            r#"{"event":{"type":"assistant_delta","text":"working"}}"#,
            r#"{"event":{"type":"reasoning_delta","text":"thinking"}}"#,
            r#"{"event":{"type":"tool_call_requested","call_id":"call_1","name":"Read"}}"#,
            r#"{"event":{"type":"approval_resolved","call_id":"call_1","approved":true}}"#,
            r#"{"event":{"type":"tool_output_delta","call_id":"call_1","stream":"stdout","text":"ok"}}"#,
            r#"{"event":{"type":"tool_finished","call_id":"call_1","status":"success"}}"#,
            r#"{"event":{"type":"error","message":"boom"}}"#,
            r#"{"event":{"type":"session_finished","outcome":"failed"}}"#,
        ];
        let state = AppState::from_events(PathBuf::from("/tmp/project"), &events);

        let output = render_to_string(&state, 80, 24);

        assert!(output.contains("approval: call_1 approved=true"));
    }

    #[test]
    fn render_to_string_should_keep_rows_within_width() {
        let events =
            [r#"{"event":{"type":"assistant_delta","text":"averyveryveryveryveryverylongtoken"}}"#];
        let state = AppState::from_events(PathBuf::from("/tmp/project"), &events);

        let output = render_to_string(&state, 44, 14);

        assert!(output.lines().all(|line| line.len() <= 44));
    }

    #[test]
    fn parse_event_line_should_render_finished_outcome() {
        let line = r#"{"event":{"type":"session_finished","outcome":"succeeded"}}"#;

        let entry = parse_event_line(line).unwrap();

        assert_eq!(entry.text, "finished succeeded");
    }

    #[test]
    fn parse_event_line_should_render_tool_finished_status() {
        let line = r#"{"event":{"type":"tool_finished","call_id":"call_1","status":"error"}}"#;

        let entry = parse_event_line(line).unwrap();

        assert_eq!(entry.text, "finished call_1 error");
    }

    #[test]
    fn parse_event_line_should_render_user_message_event() {
        let line = r#"{"event":{"type":"user_message_appended","message_id":"msg_1"}}"#;

        let entry = parse_event_line(line).unwrap();

        assert_eq!(entry.kind, TranscriptKind::User);
    }

    #[test]
    fn parse_event_line_should_render_tool_result_status_from_core_value() {
        let status = ToolResultStatus::Success.as_str();
        let line = format!(
            r#"{{"event":{{"type":"tool_finished","call_id":"call_1","status":"{status}"}}}}"#
        );

        let entry = parse_event_line(&line).unwrap();

        assert_eq!(entry.text, "finished call_1 success");
    }

    #[test]
    fn parse_event_line_should_render_outcome_from_core_value() {
        let outcome = Outcome::Succeeded.as_str();
        let line = format!(r#"{{"event":{{"type":"session_finished","outcome":"{outcome}"}}}}"#);

        let entry = parse_event_line(&line).unwrap();

        assert_eq!(entry.text, "finished succeeded");
    }

    #[test]
    fn run_task_for_state_should_render_live_task_events_and_final_status() {
        let root = temp_dir("run_task_for_state");
        fs::create_dir_all(&root).unwrap();
        let mut state = AppState::load(&root).unwrap();
        let mut runner = FakeRunner;
        let mut output = Vec::new();

        run_task_for_state(&root, &mut state, &mut runner, "list files", &mut output).unwrap();

        assert_eq!(state.status, RunStatus::Succeeded);
    }

    #[test]
    fn run_task_for_state_should_render_cancelled_status() {
        let root = temp_dir("run_task_for_state_cancel");
        fs::create_dir_all(&root).unwrap();
        let mut state = AppState::load(&root).unwrap();
        let mut runner = CancelRunner;
        let mut output = Vec::new();

        run_task_for_state(&root, &mut state, &mut runner, "list files", &mut output).unwrap();

        assert_eq!(state.status, RunStatus::Cancelled);
    }

    #[test]
    fn cancel_should_mark_state_cancelled_before_submit() {
        let mut state = empty_state();

        state.cancel();

        assert_eq!(state.status, RunStatus::Cancelled);
    }

    #[test]
    fn start_task_should_focus_transcript_on_current_task() {
        let mut state = empty_state();
        state.transcript.push(TranscriptLine {
            kind: TranscriptKind::Assistant,
            text: "old session text".to_string(),
        });

        state.start_task("new task");

        assert_eq!(state.transcript[0].text, "new task");
    }

    #[test]
    fn run_task_for_state_should_render_pending_approval_and_approved_status() {
        let root = temp_dir("run_task_for_state_approval");
        fs::create_dir_all(&root).unwrap();
        let mut state = AppState::load(&root).unwrap();
        let mut runner = ApprovalRunner;
        let mut output = Vec::new();

        run_task_for_state(
            &root,
            &mut state,
            &mut runner,
            "needs approval",
            &mut output,
        )
        .unwrap();

        assert_eq!(state.status, RunStatus::Succeeded);
    }

    struct FakeRunner;

    impl TaskRunner for FakeRunner {
        fn run_task(
            &mut self,
            _workspace_root: &Path,
            _task: &str,
            controller: &mut dyn RunController,
        ) -> Result<TuiRun, String> {
            controller.on_event(&Event::ReasoningDelta {
                text: "thinking live".to_string(),
            });
            controller.on_event(&Event::AssistantDelta {
                text: "done live".to_string(),
            });
            controller.on_event(&Event::SessionFinished {
                outcome: Outcome::Succeeded,
            });
            Ok(TuiRun {
                session_id: "session_fake".to_string(),
                outcome: "succeeded".to_string(),
            })
        }

        fn permission_mode(&mut self, _workspace_root: &Path) -> String {
            "confirm".to_string()
        }
    }

    struct CancelRunner;

    impl TaskRunner for CancelRunner {
        fn run_task(
            &mut self,
            _workspace_root: &Path,
            _task: &str,
            controller: &mut dyn RunController,
        ) -> Result<TuiRun, String> {
            controller.on_event(&Event::ModelRequestStarted {
                request_id: "request_1".to_string(),
                model: "smoke".to_string(),
            });
            let _cancel_requested = controller.should_cancel();
            controller.on_event(&Event::SessionFinished {
                outcome: Outcome::Cancelled,
            });
            Ok(TuiRun {
                session_id: "session_cancel".to_string(),
                outcome: "cancelled".to_string(),
            })
        }

        fn permission_mode(&mut self, _workspace_root: &Path) -> String {
            "confirm".to_string()
        }
    }

    struct ApprovalRunner;

    impl TaskRunner for ApprovalRunner {
        fn run_task(
            &mut self,
            _workspace_root: &Path,
            _task: &str,
            controller: &mut dyn RunController,
        ) -> Result<TuiRun, String> {
            let approved = controller.approve(&ApprovalPrompt {
                call_id: "call_approve".to_string(),
                name: "Bash".to_string(),
                input: "cargo test".to_string(),
                risk: "Execute".to_string(),
            });
            controller.on_event(&Event::ApprovalResolved {
                call_id: "call_approve".to_string(),
                approved,
            });
            controller.on_event(&Event::SessionFinished {
                outcome: Outcome::Succeeded,
            });
            Ok(TuiRun {
                session_id: "session_approve".to_string(),
                outcome: "succeeded".to_string(),
            })
        }

        fn permission_mode(&mut self, _workspace_root: &Path) -> String {
            "confirm".to_string()
        }
    }

    fn empty_state() -> AppState {
        AppState {
            workspace_root: PathBuf::from("/tmp/project"),
            sessions: Vec::new(),
            transcript: Vec::new(),
            input: String::new(),
            status: RunStatus::Idle,
            current_session_id: None,
            pending_approval: None,
            permission_mode: "confirm".to_string(),
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("flash_tui_{name}_{}", timestamp_nanos()))
    }

    fn timestamp_nanos() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    }
}
