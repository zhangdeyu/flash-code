use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event as TerminalEvent, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use flash_core::storage::load_session;
use flash_core::{discover_workspace_root, init_workspace, CancellationToken, Event};
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const DEFAULT_WIDTH: usize = 100;
const DEFAULT_HEIGHT: usize = 32;

#[async_trait(?Send)]
pub trait TaskRunner {
    fn permission_mode(&mut self, workspace_root: &Path) -> String;

    async fn run_task(
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

    fn cancellation_token(&self) -> CancellationToken {
        CancellationToken::new()
    }
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

pub async fn run_current_workspace(runner: &mut impl TaskRunner) -> Result<(), TuiError> {
    let root = discover_workspace_root(None)?;
    init_workspace(&root)?;
    let mut state = AppState::load(&root)?;
    state.permission_mode = runner.permission_mode(&root);
    let mut stdout = io::stdout();

    if let Some(session_id) = std::env::var_os("FLASH_TUI_REPLAY") {
        state.replay_session(&root, &session_id.to_string_lossy());
    }

    if let Some(task) = std::env::var_os("FLASH_TUI_TASK") {
        run_task_for_state(
            &root,
            &mut state,
            runner,
            &task.to_string_lossy(),
            &mut stdout,
        )
        .await?;
    }

    if should_render_once() {
        stdout.write_all(render_to_string(&state, DEFAULT_WIDTH, DEFAULT_HEIGHT).as_bytes())?;
        stdout.flush()?;
        return Ok(());
    }

    let _terminal = TerminalGuard::enter()?;
    render_frame(&mut stdout, &state)?;
    input_loop(&root, &mut stdout, &mut state, runner).await?;
    Ok(())
}

fn should_render_once() -> bool {
    std::env::var_os("FLASH_TUI_ONCE").is_some()
        || !io::stdin().is_terminal()
        || !io::stdout().is_terminal()
}

fn render_frame(stdout: &mut impl Write, state: &AppState) -> Result<(), TuiError> {
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| render_app(frame, state))?;
    Ok(())
}

async fn input_loop(
    workspace_root: &Path,
    stdout: &mut impl Write,
    state: &mut AppState,
    runner: &mut impl TaskRunner,
) -> Result<(), TuiError> {
    loop {
        let TerminalEvent::Key(key) = event::read()? else {
            continue;
        };
        match input_action_for_key(key, state.input.is_empty()) {
            InputAction::Cancel => {
                state.cancel();
                render_frame(stdout, state)?;
                break;
            }
            InputAction::Quit => break,
            InputAction::Submit => {
                if !state.input.is_empty() {
                    let task = state.input.clone();
                    state.input.clear();
                    if let Some(session_id) = task.strip_prefix("replay ") {
                        state.replay_session(workspace_root, session_id.trim());
                        render_frame(stdout, state)?;
                    } else {
                        run_task_for_state(workspace_root, state, runner, &task, stdout).await?;
                    }
                }
            }
            InputAction::Backspace => {
                state.input.pop();
                render_frame(stdout, state)?;
            }
            InputAction::Char(ch) => {
                state.input.push(ch);
                render_frame(stdout, state)?;
            }
            InputAction::Ignore => {}
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputAction {
    Char(char),
    Submit,
    Backspace,
    Cancel,
    Quit,
    Ignore,
}

fn input_action_for_key(key: KeyEvent, input_is_empty: bool) -> InputAction {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c' | 'C'))
    {
        return InputAction::Cancel;
    }
    match key.code {
        KeyCode::Esc => InputAction::Cancel,
        KeyCode::Enter => InputAction::Submit,
        KeyCode::Backspace => InputAction::Backspace,
        KeyCode::Char('q' | 'Q') if key.modifiers.is_empty() && input_is_empty => InputAction::Quit,
        KeyCode::Char(ch) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            InputAction::Char(ch)
        }
        _ => InputAction::Ignore,
    }
}

async fn run_task_for_state(
    workspace_root: &Path,
    state: &mut AppState,
    runner: &mut impl TaskRunner,
    task: &str,
    stdout: &mut impl Write,
) -> Result<(), TuiError> {
    state.start_task(task);
    render_frame(stdout, state)?;
    let cancellation = CancellationToken::new();
    let watcher_done = Arc::new(AtomicBool::new(false));
    let approval_active = Arc::new(AtomicBool::new(false));
    let watcher = spawn_cancellation_watcher(
        cancellation.clone(),
        Arc::clone(&watcher_done),
        Arc::clone(&approval_active),
    );
    let mut controller = UiRunController {
        state,
        stdout,
        render_error: None,
        cancellation,
        approval_active,
        last_render: Instant::now(),
    };
    let run_result = runner.run_task(workspace_root, task, &mut controller).await;
    watcher_done.store(true, Ordering::SeqCst);
    watcher
        .join()
        .map_err(|_| TuiError::Runner("cancellation watcher panicked".to_string()))?;
    let run = run_result.map_err(TuiError::Runner)?;
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
    cancellation: CancellationToken,
    approval_active: Arc<AtomicBool>,
    last_render: Instant,
}

impl<W> RunController for UiRunController<'_, W>
where
    W: Write,
{
    fn on_event(&mut self, event: &Event) {
        self.state.push_event(event);
        let stream_delta = matches!(
            event,
            Event::ReasoningDelta { .. } | Event::AssistantDelta { .. }
        );
        if !stream_delta || self.last_render.elapsed() >= Duration::from_millis(33) {
            if let Err(error) = render_frame(self.stdout, self.state) {
                self.render_error = Some(error);
            }
            self.last_render = Instant::now();
        }
    }

    fn approve(&mut self, prompt: &ApprovalPrompt) -> bool {
        self.approval_active.store(true, Ordering::SeqCst);
        self.state.set_pending_approval(prompt);
        if let Err(error) = render_frame(self.stdout, self.state) {
            self.render_error = Some(error);
            self.approval_active.store(false, Ordering::SeqCst);
            return false;
        }
        let approved =
            approval_from_env().unwrap_or_else(|| read_approval_from_stdin(&self.cancellation));
        self.approval_active.store(false, Ordering::SeqCst);
        approved
    }

    fn should_cancel(&mut self) -> bool {
        if std::env::var_os("FLASH_TUI_CANCEL_AFTER_START").is_some() {
            self.cancellation.cancel();
        }
        self.cancellation.is_cancelled()
    }

    fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

fn spawn_cancellation_watcher(
    cancellation: CancellationToken,
    done: Arc<AtomicBool>,
    approval_active: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !done.load(Ordering::SeqCst) && !cancellation.is_cancelled() {
            if approval_active.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(25));
                continue;
            }
            let Ok(ready) = event::poll(Duration::from_millis(50)) else {
                cancellation.cancel();
                return;
            };
            if !ready {
                continue;
            }
            let Ok(TerminalEvent::Key(key)) = event::read() else {
                continue;
            };
            if matches!(input_action_for_key(key, false), InputAction::Cancel) {
                cancellation.cancel();
            }
        }
    })
}

fn approval_from_env() -> Option<bool> {
    std::env::var_os("FLASH_TUI_APPROVE").map(|value| {
        matches!(
            value.to_string_lossy().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "y" | "approve"
        )
    })
}

fn read_approval_from_stdin(cancellation: &CancellationToken) -> bool {
    if !io::stdin().is_terminal() {
        return false;
    }
    loop {
        let Ok(TerminalEvent::Key(key)) = event::read() else {
            return false;
        };
        match input_action_for_key(key, false) {
            InputAction::Char('y' | 'Y') => return true,
            InputAction::Char('n' | 'N') => return false,
            InputAction::Cancel => {
                cancellation.cancel();
                return false;
            }
            _ => {}
        }
    }
}

struct TerminalGuard {
    raw_mode_enabled: bool,
    alternate_screen_entered: bool,
}

impl TerminalGuard {
    fn enter() -> Result<Self, TuiError> {
        let mut stdout = io::stdout();
        enable_raw_mode()?;
        execute!(stdout, EnterAlternateScreen, Hide)?;
        Ok(Self {
            raw_mode_enabled: true,
            alternate_screen_entered: true,
        })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        if self.alternate_screen_entered {
            let _result = execute!(stdout, Show, LeaveAlternateScreen);
        }
        if self.raw_mode_enabled {
            let _result = disable_raw_mode();
        }
    }
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
    active_stream: Option<StreamKey>,
}

impl AppState {
    pub fn load(workspace_root: &Path) -> Result<Self, TuiError> {
        flash_core::recover_workspace_sessions(workspace_root)?;
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
            active_stream: None,
        })
    }

    pub fn from_events(workspace_root: PathBuf, events: &[&str]) -> Self {
        Self {
            workspace_root,
            sessions: Vec::new(),
            transcript: transcript_from_lines(events.iter().copied()),
            input: String::new(),
            status: RunStatus::Idle,
            current_session_id: None,
            pending_approval: None,
            permission_mode: "unknown".to_string(),
            active_stream: None,
        }
    }

    fn start_task(&mut self, task: &str) {
        self.status = RunStatus::Running;
        self.current_session_id = None;
        self.pending_approval = None;
        self.active_stream = None;
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
        self.active_stream = None;
        self.transcript.push(TranscriptLine {
            kind: TranscriptKind::Session,
            text: "cancel requested".to_string(),
        });
    }

    fn push_event(&mut self, event: &Event) {
        if let Some(line) = event_to_transcript(event) {
            let stream = event_stream_key(event);
            reduce_transcript(&mut self.transcript, &mut self.active_stream, line, stream);
        }
        if matches!(event, Event::ApprovalResolved { .. }) {
            self.pending_approval = None;
        }
    }

    fn set_pending_approval(&mut self, prompt: &ApprovalPrompt) {
        self.pending_approval = Some(prompt.clone());
        self.active_stream = None;
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

    fn replay_session(&mut self, workspace_root: &Path, session_id: &str) {
        match load_session(workspace_root, session_id) {
            Ok(session) => match load_transcript(&session.path.join("events.jsonl")) {
                Ok(transcript) => {
                    self.current_session_id = Some(session.id);
                    self.status = RunStatus::Idle;
                    self.pending_approval = None;
                    self.transcript = transcript;
                    self.active_stream = None;
                }
                Err(error) => self.replace_with_error(format!("replay failed: {error}")),
            },
            Err(error) => self.replace_with_error(format!("replay failed: {error}")),
        }
    }

    fn push_error(&mut self, message: String) {
        self.status = RunStatus::Failed;
        self.active_stream = None;
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct StreamKey {
    request_id: String,
    attempt: u32,
    kind: TranscriptKind,
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
    let backend = TestBackend::new(width.max(1) as u16, height.max(1) as u16);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    terminal
        .draw(|frame| render_app(frame, state))
        .expect("test backend should render");
    buffer_to_string(terminal.backend())
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

fn wrap(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    text.split('\n')
        .flat_map(|line| split_long_line(line, width.max(1)))
        .collect()
}

fn split_long_line(text: &str, width: usize) -> Vec<String> {
    if text.width() <= width {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for ch in text.chars() {
        let character_width = ch.width().unwrap_or_default();
        if current_width + character_width > width && !current.is_empty() {
            lines.push(current);
            current = String::new();
            current_width = 0;
        }
        current.push(ch);
        current_width += character_width;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn render_app(frame: &mut Frame<'_>, state: &AppState) {
    let area = frame.area();
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(3),
            Constraint::Length(if state.pending_approval.is_some() {
                3
            } else {
                0
            }),
            Constraint::Length(3),
        ])
        .split(area);

    let session = state
        .current_session_id
        .as_ref()
        .map(|session_id| format!(" session={session_id}"))
        .unwrap_or_default();
    let header = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                "Flash Code",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("  status={}{}", state.status.as_str(), session)),
        ]),
        Line::from(format!("workspace: {}", state.workspace_root.display())),
        Line::from(format!("permission: {}", state.permission_mode)),
    ])
    .block(Block::default().borders(Borders::ALL).title("Status"));
    frame.render_widget(header, vertical[0]);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(34), Constraint::Percentage(66)])
        .split(vertical[1]);
    let sessions = if state.sessions.is_empty() {
        vec![ListItem::new("No sessions in this workspace yet.")]
    } else {
        state
            .sessions
            .iter()
            .take(8)
            .map(|session| {
                ListItem::new(format!(
                    "{}  {}  {}",
                    session.session_id, session.status, session.updated_at
                ))
            })
            .collect()
    };
    frame.render_widget(
        List::new(sessions).block(Block::default().borders(Borders::ALL).title("Sessions")),
        body[0],
    );

    let transcript = if state.transcript.is_empty() {
        vec![Line::from("No events to display.")]
    } else {
        state
            .transcript
            .iter()
            .flat_map(|entry| render_entry(entry, body[1].width.saturating_sub(2) as usize))
            .map(Line::from)
            .collect()
    };
    frame.render_widget(
        Paragraph::new(transcript)
            .block(Block::default().borders(Borders::ALL).title("Transcript"))
            .wrap(Wrap { trim: false }),
        body[1],
    );

    if let Some(prompt) = &state.pending_approval {
        let approval = Paragraph::new(format!(
            "{} {} risk={}  y approve / n reject",
            prompt.name, prompt.call_id, prompt.risk
        ))
        .block(Block::default().borders(Borders::ALL).title("Approval"));
        frame.render_widget(approval, vertical[2]);
    }

    let input = Paragraph::new(format!("{}_", state.input))
        .block(Block::default().borders(Borders::ALL).title("Input"))
        .wrap(Wrap { trim: false });
    frame.render_widget(input, vertical[3]);
}

fn buffer_to_string(backend: &TestBackend) -> String {
    let buffer = backend.buffer();
    let area = buffer.area;
    let mut output = String::new();
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            output.push_str(buffer[(x, y)].symbol());
        }
        output.push('\n');
    }
    output
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
        let Some(session_id) = json_string_field(&content, "session_id") else {
            continue;
        };
        sessions.push(SessionSummary {
            session_id,
            updated_at: json_string_field(&content, "updated_at").unwrap_or_default(),
            status: json_string_field(&content, "status").unwrap_or_else(|| "unknown".to_string()),
            events_path: entry.path().join("events.jsonl"),
        });
    }
    Ok(sessions)
}

fn load_transcript(events_path: &Path) -> Result<Vec<TranscriptLine>, TuiError> {
    let content = fs::read_to_string(events_path)?;
    Ok(transcript_from_lines(content.lines()))
}

fn transcript_from_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> Vec<TranscriptLine> {
    let mut transcript = Vec::new();
    let mut active_stream = None;
    for raw in lines {
        let Some(line) = parse_event_line(raw) else {
            continue;
        };
        reduce_transcript(
            &mut transcript,
            &mut active_stream,
            line,
            event_line_stream_key(raw),
        );
    }
    transcript
}

fn reduce_transcript(
    transcript: &mut Vec<TranscriptLine>,
    active_stream: &mut Option<StreamKey>,
    line: TranscriptLine,
    stream: Option<StreamKey>,
) {
    if stream.is_some() && stream == *active_stream {
        if let Some(previous) = transcript.last_mut() {
            previous.text.push_str(&line.text);
            return;
        }
    }
    *active_stream = stream;
    transcript.push(line);
}

fn event_stream_key(event: &Event) -> Option<StreamKey> {
    match event {
        Event::ReasoningDelta {
            request_id,
            attempt,
            ..
        } => Some(StreamKey {
            request_id: request_id.clone(),
            attempt: *attempt,
            kind: TranscriptKind::Reasoning,
        }),
        Event::AssistantDelta {
            request_id,
            attempt,
            ..
        } => Some(StreamKey {
            request_id: request_id.clone(),
            attempt: *attempt,
            kind: TranscriptKind::Assistant,
        }),
        _ => None,
    }
}

fn event_line_stream_key(line: &str) -> Option<StreamKey> {
    let kind = match json_string_field(line, "type")?.as_str() {
        "reasoning_delta" => TranscriptKind::Reasoning,
        "assistant_delta" => TranscriptKind::Assistant,
        _ => return None,
    };
    Some(StreamKey {
        request_id: json_string_field(line, "request_id").unwrap_or_default(),
        attempt: json_value_field(line, "attempt")
            .and_then(|value| value.as_u64())
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or_default(),
        kind,
    })
}

fn parse_event_line(line: &str) -> Option<TranscriptLine> {
    let event_type = json_string_field(line, "type")?;
    match event_type.as_str() {
        "session_started" => Some(TranscriptLine {
            kind: TranscriptKind::Session,
            text: format!(
                "started {}",
                json_string_field(line, "session_id").unwrap_or_default()
            ),
        }),
        "user_message_appended" => Some(TranscriptLine {
            kind: TranscriptKind::User,
            text: format!(
                "message {} appended",
                json_string_field(line, "message_id").unwrap_or_default()
            ),
        }),
        "model_request_started" => Some(TranscriptLine {
            kind: TranscriptKind::Session,
            text: format!(
                "model request {}",
                json_string_field(line, "model").unwrap_or_default()
            ),
        }),
        "reasoning_delta" => Some(TranscriptLine {
            kind: TranscriptKind::Reasoning,
            text: json_string_field(line, "text").unwrap_or_default(),
        }),
        "assistant_delta" => Some(TranscriptLine {
            kind: TranscriptKind::Assistant,
            text: json_string_field(line, "text").unwrap_or_default(),
        }),
        "model_attempt_failed" => Some(TranscriptLine {
            kind: TranscriptKind::Error,
            text: format!(
                "model attempt {} failed: {}",
                json_number_field(line, "attempt").unwrap_or_default(),
                json_string_field(line, "message").unwrap_or_default()
            ),
        }),
        "model_attempt_committed" => Some(TranscriptLine {
            kind: TranscriptKind::Session,
            text: format!(
                "model attempt {} committed",
                json_number_field(line, "attempt").unwrap_or_default()
            ),
        }),
        "tool_call_requested" => Some(TranscriptLine {
            kind: TranscriptKind::Tool,
            text: format!(
                "requested {} {}",
                json_string_field(line, "name").unwrap_or_default(),
                json_string_field(line, "call_id").unwrap_or_default()
            ),
        }),
        "approval_required" => Some(TranscriptLine {
            kind: TranscriptKind::Approval,
            text: format!(
                "required for {}",
                json_string_field(line, "call_id").unwrap_or_default()
            ),
        }),
        "approval_resolved" => Some(TranscriptLine {
            kind: TranscriptKind::Approval,
            text: format!(
                "{} approved={}",
                json_string_field(line, "call_id").unwrap_or_default(),
                json_bool_field(line, "approved").unwrap_or(false)
            ),
        }),
        "tool_started" => Some(TranscriptLine {
            kind: TranscriptKind::Tool,
            text: format!(
                "started {} {}",
                json_string_field(line, "name").unwrap_or_default(),
                json_string_field(line, "call_id").unwrap_or_default()
            ),
        }),
        "tool_output_delta" => Some(TranscriptLine {
            kind: TranscriptKind::Tool,
            text: format!(
                "{}: {}",
                json_string_field(line, "stream").unwrap_or_default(),
                json_string_field(line, "text").unwrap_or_default()
            ),
        }),
        "tool_finished" => Some(TranscriptLine {
            kind: TranscriptKind::Tool,
            text: format!(
                "finished {} {}",
                json_string_field(line, "call_id").unwrap_or_default(),
                json_string_field(line, "status").unwrap_or_default()
            ),
        }),
        "usage_recorded" => Some(TranscriptLine {
            kind: TranscriptKind::Session,
            text: format!(
                "usage input={} output={}",
                json_number_field(line, "input_tokens").unwrap_or_default(),
                json_number_field(line, "output_tokens").unwrap_or_default()
            ),
        }),
        "error" => Some(TranscriptLine {
            kind: TranscriptKind::Error,
            text: json_string_field(line, "message").unwrap_or_default(),
        }),
        "session_finished" => Some(TranscriptLine {
            kind: TranscriptKind::Session,
            text: format!(
                "finished {}",
                json_string_field(line, "outcome").unwrap_or_default()
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
        Event::ReasoningDelta { text, .. } => Some(TranscriptLine {
            kind: TranscriptKind::Reasoning,
            text: text.clone(),
        }),
        Event::AssistantDelta { text, .. } => Some(TranscriptLine {
            kind: TranscriptKind::Assistant,
            text: text.clone(),
        }),
        Event::ModelAttemptFailed {
            attempt, message, ..
        } => Some(TranscriptLine {
            kind: TranscriptKind::Error,
            text: format!("model attempt {attempt} failed: {message}"),
        }),
        Event::ModelAttemptCommitted { attempt, .. } => Some(TranscriptLine {
            kind: TranscriptKind::Session,
            text: format!("model attempt {attempt} committed"),
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
            ..
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

fn json_string_field(content: &str, key: &str) -> Option<String> {
    json_value_field(content, key).and_then(|value| value.as_str().map(str::to_string))
}

fn json_number_field(content: &str, key: &str) -> Option<String> {
    json_value_field(content, key).and_then(|value| value.as_u64().map(|number| number.to_string()))
}

fn json_bool_field(content: &str, key: &str) -> Option<bool> {
    json_value_field(content, key).and_then(|value| value.as_bool())
}

fn json_value_field(content: &str, key: &str) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    value
        .get(key)
        .cloned()
        .or_else(|| value.get("event").and_then(|event| event.get(key)).cloned())
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

        assert!(output.contains("No sessions"));
        assert!(output.contains("No events"));
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
    fn parse_event_line_should_render_attempt_transactions() {
        let failed = parse_event_line(
            r#"{"event":{"type":"model_attempt_failed","attempt":1,"message":"retry"}}"#,
        )
        .unwrap();
        let committed =
            parse_event_line(r#"{"event":{"type":"model_attempt_committed","attempt":2}}"#)
                .unwrap();

        assert_eq!(failed.kind, TranscriptKind::Error);
        assert_eq!(failed.text, "model attempt 1 failed: retry");
        assert_eq!(committed.kind, TranscriptKind::Session);
        assert_eq!(committed.text, "model attempt 2 committed");
    }

    #[test]
    fn transcript_reducer_should_merge_adjacent_stream_deltas_without_changing_whitespace() {
        let events = [
            r#"{"event":{"type":"assistant_delta","request_id":"req_1","attempt":1,"text":"hello\n"}}"#,
            r#"{"event":{"type":"assistant_delta","request_id":"req_1","attempt":1,"text":"  world"}}"#,
        ];

        let state = AppState::from_events(PathBuf::from("/tmp/project"), &events);

        assert_eq!(state.transcript.len(), 1);
        assert_eq!(state.transcript[0].text, "hello\n  world");
    }

    #[test]
    fn live_and_replay_should_use_identical_stream_reduction() {
        let mut live = empty_state();
        live.push_event(&Event::AssistantDelta {
            request_id: "req_1".to_string(),
            attempt: 1,
            text: "hel".to_string(),
        });
        live.push_event(&Event::AssistantDelta {
            request_id: "req_1".to_string(),
            attempt: 1,
            text: "lo".to_string(),
        });
        let replay = AppState::from_events(
            PathBuf::from("/tmp/project"),
            &[
                r#"{"event":{"type":"assistant_delta","request_id":"req_1","attempt":1,"text":"hel"}}"#,
                r#"{"event":{"type":"assistant_delta","request_id":"req_1","attempt":1,"text":"lo"}}"#,
            ],
        );

        assert_eq!(live.transcript, replay.transcript);
    }

    #[test]
    fn app_state_load_should_scan_sessions_without_index() {
        let root = temp_dir("scan_sessions");
        fs::create_dir_all(&root).unwrap();
        let session = create_session(&root).unwrap();
        append_event(
            &session,
            Event::AssistantDelta {
                request_id: "request_1".to_string(),
                attempt: 1,
                text: "hello from replay".to_string(),
            },
        )
        .unwrap();

        let state = AppState::load(&root).unwrap();

        assert_eq!(state.sessions.len(), 1);
    }

    #[test]
    fn replay_session_should_load_transcript_for_current_workspace() {
        let root = temp_dir("replay_current");
        fs::create_dir_all(&root).unwrap();
        let session = create_session(&root).unwrap();
        append_event(
            &session,
            Event::AssistantDelta {
                request_id: "request_1".to_string(),
                attempt: 1,
                text: "hello replay".to_string(),
            },
        )
        .unwrap();
        let mut state = AppState::load(&root).unwrap();

        state.replay_session(&root, &session.id);

        assert_eq!(state.current_session_id, Some(session.id));
    }

    #[test]
    fn replay_session_should_render_workspace_mismatch_error() {
        let root = temp_dir("replay_a");
        let other = temp_dir("replay_b");
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

        state.replay_session(&other, &session.id);

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
    fn render_to_string_should_match_key_state_snapshot() {
        let events = [
            r#"{"event":{"type":"assistant_delta","text":"streaming answer"}}"#,
            r#"{"event":{"type":"reasoning_delta","text":"inspect workspace"}}"#,
            r#"{"event":{"type":"tool_call_requested","call_id":"call_1","name":"Bash"}}"#,
            r#"{"event":{"type":"tool_output_delta","call_id":"call_1","stream":"stdout","text":"test ok"}}"#,
            r#"{"event":{"type":"approval_resolved","call_id":"call_1","approved":true}}"#,
            r#"{"event":{"type":"error","message":"sample error"}}"#,
        ];
        let mut state = AppState::from_events(PathBuf::from("/tmp/project"), &events);
        state.status = RunStatus::Running;
        state.current_session_id = Some("session_snapshot".to_string());
        state.permission_mode = "confirm".to_string();
        state.pending_approval = Some(ApprovalPrompt {
            call_id: "call_2".to_string(),
            name: "Edit".to_string(),
            input: "src/lib.rs".to_string(),
            risk: "Write".to_string(),
        });

        let output = render_to_string(&state, 72, 22);

        assert_eq!(
            output,
            include_str!("../tests/golden/key_state_snapshot.txt")
        );
    }

    #[test]
    fn render_to_string_should_keep_rows_within_width() {
        let events =
            [r#"{"event":{"type":"assistant_delta","text":"averyveryveryveryveryverylongtoken"}}"#];
        let state = AppState::from_events(PathBuf::from("/tmp/project"), &events);

        let output = render_to_string(&state, 44, 14);

        assert!(output.lines().all(|line| line.width() <= 44));
    }

    #[test]
    fn wrap_should_preserve_code_whitespace_and_use_cjk_display_width() {
        let text = "assistant: ```rust\n  let  value = 1;\n\n中文中文";

        let lines = wrap(text, 10);

        assert_eq!(
            lines,
            vec![
                "assistant:",
                " ```rust",
                "  let  val",
                "ue = 1;",
                "",
                "中文中文"
            ]
        );
        assert!(lines.iter().all(|line| line.width() <= 10));
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
    fn input_action_should_cover_crossterm_keyboard_controls() {
        assert_eq!(
            input_action_for_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), false),
            InputAction::Char('a')
        );
        assert_eq!(
            input_action_for_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), false),
            InputAction::Submit
        );
        assert_eq!(
            input_action_for_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), false),
            InputAction::Backspace
        );
        assert_eq!(
            input_action_for_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), false),
            InputAction::Cancel
        );
        assert_eq!(
            input_action_for_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                false
            ),
            InputAction::Cancel
        );
        assert_eq!(
            input_action_for_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), true),
            InputAction::Quit
        );
        assert_eq!(
            input_action_for_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), false),
            InputAction::Char('q')
        );
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

    #[tokio::test]
    async fn run_task_for_state_should_render_live_task_events_and_final_status() {
        let root = temp_dir("run_task_for_state");
        fs::create_dir_all(&root).unwrap();
        let mut state = AppState::load(&root).unwrap();
        let mut runner = FakeRunner;
        let mut output = Vec::new();

        run_task_for_state(&root, &mut state, &mut runner, "list files", &mut output)
            .await
            .unwrap();

        assert_eq!(state.status, RunStatus::Succeeded);
    }

    #[tokio::test]
    async fn run_task_for_state_should_render_cancelled_status() {
        let root = temp_dir("run_task_for_state_cancel");
        fs::create_dir_all(&root).unwrap();
        let mut state = AppState::load(&root).unwrap();
        let mut runner = CancelRunner;
        let mut output = Vec::new();

        run_task_for_state(&root, &mut state, &mut runner, "list files", &mut output)
            .await
            .unwrap();

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

    #[tokio::test]
    async fn run_task_for_state_should_render_pending_approval_and_approved_status() {
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
        .await
        .unwrap();

        assert_eq!(state.status, RunStatus::Succeeded);
    }

    struct FakeRunner;

    #[async_trait(?Send)]
    impl TaskRunner for FakeRunner {
        async fn run_task(
            &mut self,
            _workspace_root: &Path,
            _task: &str,
            controller: &mut dyn RunController,
        ) -> Result<TuiRun, String> {
            controller.on_event(&Event::ReasoningDelta {
                request_id: "request_1".to_string(),
                attempt: 1,
                text: "thinking live".to_string(),
            });
            controller.on_event(&Event::AssistantDelta {
                request_id: "request_1".to_string(),
                attempt: 1,
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

    #[async_trait(?Send)]
    impl TaskRunner for CancelRunner {
        async fn run_task(
            &mut self,
            _workspace_root: &Path,
            _task: &str,
            controller: &mut dyn RunController,
        ) -> Result<TuiRun, String> {
            controller.on_event(&Event::ModelRequestStarted {
                request_id: "request_1".to_string(),
                attempt: 1,
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

    #[async_trait(?Send)]
    impl TaskRunner for ApprovalRunner {
        async fn run_task(
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
            active_stream: None,
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
