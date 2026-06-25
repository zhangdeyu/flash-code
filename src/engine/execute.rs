use std::collections::HashMap;

use futures::future::BoxFuture;

use crate::engine::approval::ApprovalCallback;
use crate::engine::invoke::invoke_tool;
use crate::engine::run_context::RunContext;
use crate::protocol::{
    ApprovalDecision, ContentBlock, Event, RiskLevel, ToolApprovalAdvice, ToolCall,
};
use crate::tool::{ApprovalGate, ApprovalOutcome, ExecutionContext, ToolRegistry};

pub enum ToolPhaseOutcome {
    Executed(Vec<ContentBlock>),
    Cancelled(Vec<ContentBlock>),
    AllRejected(Vec<ContentBlock>),
}

fn summarize_input(call: &ToolCall) -> String {
    if let Some(s) = call.input.get("command").and_then(|v| v.as_str()) {
        return s.to_owned();
    }
    serde_json::to_string(&call.input).unwrap_or_default()
}

pub async fn run_tool_phase(
    calls: &[ToolCall],
    registry: &ToolRegistry,
    approval: &ApprovalCallback,
    ctx: &RunContext,
) -> ToolPhaseOutcome {
    let mut approved: Vec<ToolCall> = Vec::new();
    let mut rejected: Vec<ToolCall> = Vec::new();

    // ---- per-tool approval, sequential ----
    for call in calls {
        let tool_opt = registry.get(&call.name);
        let advice = match tool_opt {
            Some(t) => t.approval_advice(&call.input),
            None => ToolApprovalAdvice {
                decision: ApprovalDecision::Default,
                reason: None,
            },
        };
        let risk = tool_opt.map_or(RiskLevel::Dangerous, |t| t.spec().risk);

        let gate = ApprovalGate {
            mode: ctx.approval_mode,
        };
        let outcome = gate.decide(risk, advice);
        let approved_this = match outcome {
            ApprovalOutcome::AutoApprove => {
                ctx.sink
                    .emit(Event::ApprovalGranted {
                        session_id: ctx.session_id.clone(),
                        call_id: call.call_id.clone(),
                    })
                    .await;
                true
            }
            ApprovalOutcome::Ask => {
                ctx.sink
                    .emit(Event::ApprovalRequired {
                        session_id: ctx.session_id.clone(),
                        call_id: call.call_id.clone(),
                        command: summarize_input(call),
                    })
                    .await;
                let r = (approval)(call, ctx).await;
                ctx.sink
                    .emit(if r {
                        Event::ApprovalGranted {
                            session_id: ctx.session_id.clone(),
                            call_id: call.call_id.clone(),
                        }
                    } else {
                        Event::ApprovalRejected {
                            session_id: ctx.session_id.clone(),
                            call_id: call.call_id.clone(),
                        }
                    })
                    .await;
                r
            }
        };

        if approved_this {
            approved.push(call.clone());
        } else {
            rejected.push(call.clone());
        }
    }

    if approved.is_empty() {
        let results = rejected
            .into_iter()
            .map(|c| ContentBlock::ToolResult {
                call_id: c.call_id,
                content: vec![ContentBlock::text("user rejected this tool call")],
                is_error: true,
            })
            .collect();
        return ToolPhaseOutcome::AllRejected(results);
    }

    // ---- parallel execution under shared child cancel ----
    let group_cancel = ctx.cancel.child_token();
    let mut futs: Vec<BoxFuture<'_, ContentBlock>> = Vec::with_capacity(approved.len());

    for call in &approved {
        match registry.get(&call.name) {
            Some(tool) => {
                let exec_ctx = ExecutionContext {
                    session_id: ctx.session_id.clone(),
                    call_id: call.call_id.clone(),
                    cwd: ctx.cwd.clone(),
                    timeout: ctx.tool_timeout,
                    max_output_bytes: ctx.tool_max_output_bytes,
                    cancel: group_cancel.clone(),
                    sink: ctx.sink.clone(),
                };
                let call = call.clone();
                let sink = ctx.sink.clone();
                let session_id = ctx.session_id.clone();
                let tool = tool.clone();
                futs.push(Box::pin(async move {
                    invoke_tool(&*tool, call, &exec_ctx, sink, session_id).await
                }));
            }
            None => {
                let block = ContentBlock::ToolResult {
                    call_id: call.call_id.clone(),
                    content: vec![ContentBlock::text(format!("unknown tool: {}", call.name))],
                    is_error: true,
                };
                let sink = ctx.sink.clone();
                let session_id = ctx.session_id.clone();
                let call_id = call.call_id.clone();
                let name = call.name.clone();
                futs.push(Box::pin(async move {
                    sink.emit(Event::ToolError {
                        session_id,
                        call_id,
                        error: format!("unknown tool: {name}"),
                    })
                    .await;
                    block
                }));
            }
        }
    }

    let results = futures::future::join_all(futs).await;

    // ---- merge in original call order ----
    let mut by_id: HashMap<String, ContentBlock> = HashMap::new();
    for r in results {
        if let ContentBlock::ToolResult { call_id, .. } = &r {
            by_id.insert(call_id.clone(), r);
        }
    }
    for c in &rejected {
        by_id.insert(
            c.call_id.clone(),
            ContentBlock::ToolResult {
                call_id: c.call_id.clone(),
                content: vec![ContentBlock::text("user rejected this tool call")],
                is_error: true,
            },
        );
    }

    let merged: Vec<ContentBlock> = calls
        .iter()
        .filter_map(|c| by_id.remove(&c.call_id))
        .collect();

    if ctx.cancel.is_cancelled() {
        ToolPhaseOutcome::Cancelled(merged)
    } else {
        ToolPhaseOutcome::Executed(merged)
    }
}
