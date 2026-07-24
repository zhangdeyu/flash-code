# Flash Code 数据流转方案

本文档定义 v1 数据流。目标是把 streaming、tool call、session 存储和 replay 讲清楚,不提前设计完整 telemetry 或独立 trajectory 系统。

## 1. 总览

```text
User Input
  -> UserAction
  -> AgentRuntime
  -> Prompt from messages.jsonl
  -> Provider
  -> ProviderEvent stream
  -> AgentRuntime buffer
  -> Event stream
  -> events.jsonl + CLI/TUI renderer
  -> committed Message
  -> messages.jsonl
```

工具调用:

```text
ToolCallComplete
  -> Tool Registry
  -> Permission Policy
  -> Approval if needed
  -> Tool Execution
  -> Tool events
  -> ToolResult message
  -> messages.jsonl
```

## 2. 三类核心数据

### 2.1 UserAction

前端输入统一成 `UserAction`。

```rust
pub enum UserAction {
    SubmitPrompt { text: String },
    ApproveTool { call_id: String },
    RejectTool { call_id: String },
    Cancel,
    ResumeSession { session_id: String },
}
```

来源:

- headless CLI
- TUI
- eval adapter

约束:

- 不包含 DeepSeek 专属字段。
- `ResumeSession` 必须先做 workspace 校验。

### 2.2 ProviderEvent

Provider 把模型原始流转换为 provider-agnostic 事件。

```rust
pub enum ProviderEvent {
    ReasoningDelta(String),
    TextDelta(String),
    ToolCallComplete(ToolCall),
    Usage(Usage),
    Done(StopReason),
}
```

DeepSeek SSE 只在 `deepseek` crate 内解析。Agent runtime 不读取 DeepSeek 原始 JSON。

### 2.3 Event

Agent runtime 对外只发 `Event`。

```rust
pub enum Event {
    SessionStarted { session_id: String },
    UserMessageAppended { message_id: String },
    ModelRequestStarted { request_id: String, model: String },
    ReasoningDelta { text: String },
    AssistantDelta { text: String },
    AssistantMessageCompleted { message_id: String },
    ToolCallRequested { call_id: String, name: String },
    ApprovalRequired { call_id: String },
    ApprovalResolved { call_id: String, approved: bool },
    ToolStarted { call_id: String, name: String },
    ToolOutputDelta { call_id: String, stream: String, text: String },
    ToolFinished { call_id: String, status: ToolResultStatus },
    UsageRecorded { usage: Usage },
    Error { message: String },
    SessionFinished { outcome: Outcome },
}
```

`Event` 用于:

- CLI 输出。
- TUI 状态更新。
- `events.jsonl` 落盘。
- replay。
- 早期 golden tests。

## 3. 模型流式输出

流式输出不能直接写入 `messages.jsonl`。

原因:

- streaming 期间输出可能被取消。
- provider 可能中途报错。
- tool call arguments 可能分片到达。
- 半截 assistant message 不能进入下一轮 prompt。

runtime 必须先放入 buffer:

```rust
pub struct AssistantBuffer {
    pub reasoning: String,
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub stop_reason: Option<StopReason>,
}
```

处理规则:

- `ReasoningDelta` -> 写 `Event::ReasoningDelta`,追加到 buffer。
- `TextDelta` -> 写 `Event::AssistantDelta`,追加到 buffer。
- `ToolCallComplete` -> 写 `Event::ToolCallRequested`,追加到 buffer。
- `Usage` -> 写 `Event::UsageRecorded`。
- `Done` -> 根据 buffer commit assistant message 或结束。

commit 规则:

- 正常完成后,assistant message 写入 `messages.jsonl`。
- 如果 stream 中取消,丢弃 buffer,不写 `messages.jsonl`。
- 如果 provider 中途失败,丢弃 buffer,写 error event。
- 如果有 tool call,commit assistant 后进入工具流程。

## 4. 工具调用

```text
AssistantBuffer.tool_calls
  -> commit assistant message
  -> resolve tool from registry
  -> compute risk
  -> apply permission policy
  -> approval if policy asks
  -> execute tool
  -> commit tool result message
```

工具事件:

- `ToolStarted`
- `ToolOutputDelta`
- `ToolFinished`
- `Error`

工具结果:

- success
- error
- rejected
- cancelled

不变量:

- assistant message 一旦包含 tool call,就必须有对应 tool result。
- rejected/cancelled 也要写 tool result。
- unknown tool 不 panic,写 error tool result。
- 工具执行前必须经过 permission policy。
- `human` 模式下不执行真实工具,只写建议和对应 tool result。
- 多个 tool call 在 v1 按稳定顺序执行。

权限决策:

```text
tool input
  -> Tool::risk(input)
  -> PermissionPolicy::decide(risk, approval_mode, workspace)
  -> allow | ask | deny
```

`ask` 会发出 `ApprovalRequired`。用户在 CLI/TUI 中确认或拒绝后,再写入 `ApprovalResolved`。

## 4.1 Agent Loop 数据边界

Agent loop 每轮只允许一个稳定的数据提交点:

```text
model stream done -> commit assistant
tool terminal state -> commit tool result
turn finished -> update session metadata
```

loop 不把 streaming 中间态写入 `messages.jsonl`,只写入 `events.jsonl`。这样 resume 只依赖 committed history,replay 才展示中间过程。

## 5. 存储写入

v1 只强制两个 append-only 文件:

```text
messages.jsonl
events.jsonl
```

用户输入:

```text
write user message -> messages.jsonl
write UserMessageAppended -> events.jsonl
```

模型 streaming:

```text
write delta events -> events.jsonl
on done write assistant message -> messages.jsonl
write AssistantMessageCompleted -> events.jsonl
```

工具执行:

```text
write tool events -> events.jsonl
write artifacts if output is large
write tool result message -> messages.jsonl
```

`session.json` 在状态变化后更新元数据。

## 6. Replay

Replay 只读取 `events.jsonl`。

```text
flash replay .flash/sessions/session_xxx/events.jsonl
  -> EventLogReader
  -> renderer
```

Replay 不调用模型、不执行工具、不要求当前目录等于原 workspace。

## 7. Resume

Resume 只读取 `messages.jsonl` 重建 History。

```text
flash resume session_xxx
  -> resolve current workspace
  -> read session.json
  -> verify workspace
  -> read messages.jsonl
  -> rebuild History
  -> continue
```

如果 `events.jsonl` 有 assistant delta,但 `messages.jsonl` 没有 assistant message,说明上次运行中断。resume 时忽略这段半截输出。

## 8. TUI 数据流

TUI 不直接调用 Provider 或 Tool。

```text
keyboard
  -> UserAction
  -> AgentRuntime
  -> Event
  -> TuiState
  -> render
```

TUI 可以:

- 从 `events.jsonl` 重建 transcript。
- 从 `session.json` 列当前 workspace sessions。
- 发送 approve/reject/cancel。

TUI 不可以:

- 直接执行 shell。
- 直接修改 History。
- 绕过 workspace resume 校验。

## 9. Eval 数据流

Eval 也是 runtime 的调用方。

```text
benchmark task
  -> UserAction::SubmitPrompt
  -> AgentRuntime
  -> events.jsonl
  -> grader/report
```

v1 不需要独立 trajectory。到 Terminal-Bench/SWE-bench 阶段,如果 `events.jsonl` 不够再派生 `trajectory.jsonl`。

## 10. 后续扩展

只有出现真实需求时再加:

- 独立 `trajectory.jsonl`。
- 复杂 telemetry。
- compaction event。
- event channel 背压优化。
- 更细的 artifact manifest。

v1 先保证数据流正确、可恢复、可 replay。
