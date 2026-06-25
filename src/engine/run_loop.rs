use crate::engine::approval::ApprovalCallback;
use crate::engine::compact::{maybe_compact, overflow_compact, CompactionPolicy};
use crate::engine::execute::{run_tool_phase, ToolPhaseOutcome};
use crate::engine::loop_transition::LoopTransition;
use crate::engine::run_context::RunContext;
use crate::engine::stream::{stream_model, StreamError};
use crate::error::{Error, Result};
use crate::protocol::{ContentBlock, Event, Message, Prompt, ToolCall};
use crate::provider::{Provider, ProviderError, StopReason};
use crate::session::Session;
use crate::tool::ToolRegistry;

pub const MAX_TURNS: usize = 50;

fn extract_tool_uses(blocks: &[ContentBlock]) -> Vec<ToolCall> {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse {
                call_id,
                name,
                input,
            } => Some(ToolCall {
                call_id: call_id.clone(),
                name: name.clone(),
                input: input.clone(),
            }),
            _ => None,
        })
        .collect()
}

async fn finalize_cancelled(ctx: &RunContext, reason: &str) -> Result<()> {
    ctx.sink
        .emit(Event::Cancelled {
            session_id: ctx.session_id.clone(),
            reason: reason.to_owned(),
        })
        .await;
    Ok(())
}

async fn emit_message_appended(ctx: &RunContext, message: Message) {
    ctx.sink
        .emit(Event::MessageAppended {
            session_id: ctx.session_id.clone(),
            message,
        })
        .await;
}

async fn emit_micro_if_any(session: &Session, ctx: &RunContext) {
    let (_, r) = session.history.project_with_micro();
    if !r.redacted_ids.is_empty() {
        ctx.sink
            .emit(Event::MicroCompacted {
                session_id: ctx.session_id.clone(),
                redacted_ids: r.redacted_ids,
                bytes_saved: r.bytes_saved,
            })
            .await;
    }
}

pub async fn run_loop(
    session: &mut Session,
    user_input: Vec<ContentBlock>,
    provider: &dyn Provider,
    registry: &ToolRegistry,
    compaction_policy: &dyn CompactionPolicy,
    approval_callback: ApprovalCallback,
    ctx: &RunContext,
) -> Result<()> {
    let user_id = session.history.push_user(user_input);
    let user_msg = session
        .history
        .raw_messages()
        .iter()
        .find(|m| m.id == user_id)
        .cloned()
        .expect("user message just pushed");
    emit_message_appended(ctx, user_msg).await;

    let mut transition = LoopTransition::Initial;
    let mut turns = 0_usize;
    let mut max_output_override: Option<usize> = None;

    loop {
        if ctx.cancel.is_cancelled() {
            return finalize_cancelled(ctx, "before model call").await;
        }

        // ---- 1. compaction (degrades on failure) ----
        if let Err(e) = maybe_compact(
            &mut session.history,
            compaction_policy,
            provider,
            provider.capability(),
            ctx.sink.clone(),
            &ctx.session_id,
        )
        .await
        {
            ctx.sink
                .emit(Event::Error {
                    session_id: ctx.session_id.clone(),
                    message: format!("compaction skipped: {e}"),
                })
                .await;
        }

        emit_micro_if_any(session, ctx).await;

        // ---- 2. assemble prompt + stream ----
        let prompt = Prompt {
            system: session.system.clone(),
            tools: registry.specs(),
            messages: session.history.to_prompt_messages(),
        };

        let _ = max_output_override; // kept for future use if provider needs it

        let stream_result = stream_model(
            &prompt,
            provider,
            &ctx.cancel,
            &ctx.session_id,
            ctx.sink.clone(),
        )
        .await;

        let (assistant_blocks, stop_reason) = match stream_result {
            Ok(r) => r,
            Err(StreamError::Provider(ProviderError::ContextOverflow(_)))
                if !matches!(transition, LoopTransition::OverflowRetried) =>
            {
                overflow_compact(
                    &mut session.history,
                    compaction_policy,
                    provider,
                    ctx.sink.clone(),
                    &ctx.session_id,
                )
                .await?;
                transition = LoopTransition::OverflowRetried;
                continue;
            }
            Err(StreamError::Provider(e)) if e.is_retryable() && !matches!(
                transition,
                LoopTransition::TransientRetried
            ) =>
            {
                if let ProviderError::RateLimited {
                    retry_after: Some(d),
                    ..
                } = &e
                {
                    tokio::time::sleep(*d).await;
                }
                transition = LoopTransition::TransientRetried;
                continue;
            }
            Err(StreamError::Cancelled) => {
                return finalize_cancelled(ctx, "model streaming").await;
            }
            Err(StreamError::Provider(e)) => return Err(Error::Provider(e)),
        };

        // ---- 3. MaxTokens 升级重试一次 ----
        if stop_reason == StopReason::MaxTokens
            && !matches!(transition, LoopTransition::MaxTokensRetried)
        {
            max_output_override = Some(provider.capability().max_output * 4);
            transition = LoopTransition::MaxTokensRetried;
            continue;
        }
        max_output_override = None;

        // ---- 4. push assistant ----
        let asst_id = session.history.push_assistant(assistant_blocks.clone());
        let asst_msg = session
            .history
            .raw_messages()
            .iter()
            .find(|m| m.id == asst_id)
            .cloned()
            .expect("assistant message just pushed");
        emit_message_appended(ctx, asst_msg).await;

        // ---- 5. terminal? ----
        let tool_calls = extract_tool_uses(&assistant_blocks);
        if tool_calls.is_empty() {
            return Ok(());
        }

        // ---- 6. turn count (only when entering tool phase) ----
        turns += 1;
        if turns >= MAX_TURNS {
            ctx.sink
                .emit(Event::Error {
                    session_id: ctx.session_id.clone(),
                    message: "max turns exceeded".into(),
                })
                .await;
            return Err(Error::MaxTurnsExceeded);
        }

        // ---- 7. tool phase ----
        let outcome = run_tool_phase(&tool_calls, registry, &approval_callback, ctx).await;
        match outcome {
            ToolPhaseOutcome::Executed(results) => {
                let tool_id = session.history.push_tool_results(results)?;
                let tool_msg = session
                    .history
                    .raw_messages()
                    .iter()
                    .find(|m| m.id == tool_id)
                    .cloned()
                    .expect("tool message just pushed");
                emit_message_appended(ctx, tool_msg).await;
                transition = LoopTransition::ToolResultReturn;
            }
            ToolPhaseOutcome::AllRejected(results) => {
                let tool_id = session.history.push_tool_results(results)?;
                let tool_msg = session
                    .history
                    .raw_messages()
                    .iter()
                    .find(|m| m.id == tool_id)
                    .cloned()
                    .expect("tool message just pushed");
                emit_message_appended(ctx, tool_msg).await;
                transition = LoopTransition::ToolResultReturn;
            }
            ToolPhaseOutcome::Cancelled(partial) => {
                let _ = session.history.push_tool_results(partial);
                return finalize_cancelled(ctx, "tool execution").await;
            }
        }
    }
}
