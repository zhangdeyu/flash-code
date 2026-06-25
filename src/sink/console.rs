use std::io::Write;

use async_trait::async_trait;

use crate::protocol::Event;
use crate::sink::EventSink;

/// Renders events as human-readable text to stdout.
///
/// Uses `std::sync::Mutex<Stdout>` — no `.await` inside the critical section,
/// so `clippy::await_holding_lock` does not apply.
pub struct ConsoleSink {
    out: std::sync::Mutex<std::io::Stdout>,
}

impl ConsoleSink {
    #[must_use]
    pub fn new() -> Self {
        Self {
            out: std::sync::Mutex::new(std::io::stdout()),
        }
    }
}

impl Default for ConsoleSink {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EventSink for ConsoleSink {
    async fn emit(&self, event: Event) {
        let Ok(mut out) = self.out.lock() else {
            return;
        };
        let line = render_human_readable(&event);
        // Swallow write errors — sink should not crash the tool loop
        let _ = writeln!(out, "{line}");
    }
}

/// Render an event as a single human-readable line.
fn render_human_readable(event: &Event) -> String {
    match event {
        Event::SessionStarted { session_id } => format!("[session] started {session_id}"),
        Event::AssistantMessageStart { .. } => "[assistant] ...".to_owned(),
        Event::AssistantToken { text, .. } => text.clone(),
        Event::AssistantMessageEnd { .. } => String::new(),
        Event::ToolStart { tool, call_id, .. } => format!("[tool] {tool} ({call_id}) started"),
        Event::ToolEnd { call_id, duration_ms, .. } => format!("[tool] {call_id} done ({duration_ms}ms)"),
        Event::ToolError { call_id, error, .. } => format!("[tool] {call_id} error: {error}"),
        Event::ToolCancelled { call_id, .. } => format!("[tool] {call_id} cancelled"),
        Event::ApprovalRequired { command, .. } => format!("[approval] required: {command}"),
        Event::ApprovalGranted { call_id, .. } => format!("[approval] granted {call_id}"),
        Event::ApprovalRejected { call_id, .. } => format!("[approval] rejected {call_id}"),
        Event::HistoryCompacted {
            before_count,
            tail_count,
            ..
        } => {
            format!("[compact] {before_count} raw -> {tail_count} tail (+ summary)")
        }
        Event::MicroCompacted {
            redacted_ids,
            bytes_saved,
            ..
        } => format!(
            "[micro-compact] redacted {} tool_results, saved {} bytes",
            redacted_ids.len(),
            bytes_saved
        ),
        Event::MessageAppended { message, .. } => {
            format!("[message] role={:?} id={}", message.role, message.id)
        }
        Event::Cancelled { reason, .. } => format!("[cancelled] {reason}"),
        Event::Error { message, .. } => format!("[error] {message}"),
        Event::Unknown(_) => String::new(),
    }
}
// ConsoleSink: human-readable output
