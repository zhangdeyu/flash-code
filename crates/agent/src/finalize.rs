use std::path::Path;

use flash_core::{append_event_async, finalize_session_async, Event, Outcome, ToolRegistry};

use crate::error::AgentRun;
use crate::hooks::EventObserver;

pub(crate) async fn emit_event(
    session: &flash_core::storage::Session,
    event: Event,
    observer: &mut impl EventObserver,
) -> Result<(), crate::error::AgentError> {
    append_event_async(session.clone(), event.clone()).await?;
    observer.on_event(&event);
    Ok(())
}

pub(crate) async fn finish_session(
    session: &flash_core::storage::Session,
    outcome: Outcome,
    observer: &mut impl EventObserver,
) -> Result<AgentRun, crate::error::AgentError> {
    if session.begin_finalize() {
        let event_result = emit_event(session, Event::SessionFinished { outcome }, observer).await;
        let metadata_result = finalize_session_async(session.clone(), outcome).await;
        event_result?;
        metadata_result?;
    }
    Ok(AgentRun {
        session_id: session.id.clone(),
        outcome,
    })
}

pub(crate) fn runtime_system_prompt(workspace_root: &Path, tools: &ToolRegistry) -> String {
    let available_tools = tools.names().collect::<Vec<_>>().join(", ");
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "unknown".to_string());
    format!(
        "{}\n\n# Runtime environment\n\n- Operating system: {}\n- Shell: {}\n- Working directory: {}\n- Available tools: {}",
        include_str!("../../../prompts/system_default.md").trim_end(),
        std::env::consts::OS,
        shell,
        workspace_root.display(),
        available_tools
    )
}
