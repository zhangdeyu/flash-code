use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use flash_core::{discover_workspace_root, init_workspace};

const DEFAULT_WIDTH: usize = 100;
const DEFAULT_HEIGHT: usize = 32;

pub fn run_current_workspace() -> Result<(), TuiError> {
    let root = discover_workspace_root(None)?;
    init_workspace(&root)?;
    let state = AppState::load(&root)?;
    let mut stdout = io::stdout();

    if should_render_once() {
        stdout.write_all(render_to_string(&state, DEFAULT_WIDTH, DEFAULT_HEIGHT).as_bytes())?;
        stdout.flush()?;
        return Ok(());
    }

    let _terminal = TerminalGuard::enter()?;
    render_frame(&mut stdout, &state)?;
    wait_for_quit()?;
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

fn wait_for_quit() -> Result<(), TuiError> {
    let mut stdin = io::stdin();
    let mut buffer = [0_u8; 1];
    loop {
        let read = stdin.read(&mut buffer)?;
        if read == 0 || matches!(buffer[0], b'q' | b'Q' | 3 | 27) {
            break;
        }
    }
    Ok(())
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
}

impl std::fmt::Display for TuiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "tui io error: {error}"),
            Self::CoreWorkspace(error) => write!(formatter, "{error}"),
            Self::CoreStorage(error) => write!(formatter, "{error}"),
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
        let state = AppState {
            workspace_root: PathBuf::from("/tmp/project"),
            sessions: Vec::new(),
            transcript: Vec::new(),
        };

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
