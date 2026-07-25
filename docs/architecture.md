# Flash Code 架构方案

本文档定义 v1 架构。原则是克制:只设计当前阶段需要稳定的边界,把可以后置的能力明确标成后续扩展。

## 1. 产品形态

Flash Code 保留两种入口:

- TUI:主交互界面,0.4 起无参数 `flash` 默认进入 TUI。
- Headless CLI:自动化、评测、replay、doctor、CI、单次任务。

不做:

- Web UI。
- 主要交互式 CLI chat。
- 评测专用 Agent runtime。

## 2. 核心原则

```text
Core Protocol: provider-agnostic
Provider Adapter: provider-specific
Runtime Policy: capability-aware
Product Default: DeepSeek-first
```

含义:

- 核心协议不绑定 DeepSeek。
- DeepSeek 优化放在 DeepSeek Provider 内。
- Agent runtime 只读取模型能力,不写 `if provider == "deepseek"`。
- CLI、TUI、Eval 共享同一个 runtime。
- Session 绑定 workspace,A 目录创建的 session 默认只能在 A 目录 resume。

入口分发:

```text
flash                  -> TUI
flash tui              -> TUI explicit alias
flash run "<task>"     -> headless CLI
flash eval ...         -> benchmark/eval CLI
flash replay ...       -> replay CLI
flash doctor           -> diagnostics CLI
```

0.4 之前 TUI 尚未实现时,无参数 `flash` 可以临时显示帮助和当前可用命令。

## 3. v1 Workspace 结构

v1 不需要过多 crate。先按 6 个核心 crate 起步:

```text
crates/
  core/       # Message、Event、History、Session metadata
  provider/   # Provider trait、ProviderEvent、ModelCapabilities、ProviderError
  deepseek/   # DeepSeek adapter
  agent/      # Agent runtime、tool loop、approval、prompt projection
  tools/      # shell、file、search、git/test 后续加入
  cli/        # flash run / replay / doctor
```

0.4 引入:

```text
crates/tui/
```

0.6 引入:

```text
crates/eval/
```

暂不单独拆:

- `sandbox`:先放在 `tools` 内,等 PTY/权限复杂后再拆。
- `telemetry`:先从 `events.jsonl` 派生,等评测报表需要再拆。
- `xtask`:CI 命令变复杂后再加。

## 4. 依赖方向

```text
core
  ^
  |
provider    tools
  ^          ^
  |          |
deepseek    |
   \         |
    \        |
      agent
        ^
        |
      cli
        ^
        |
      tui   eval
```

约束:

- `core` 不依赖 DeepSeek、HTTP、shell、TUI。
- `deepseek` 只负责模型调用适配,不执行工具,不改 History。
- `tools` 不调用模型,不直接写 session 文件。
- `agent` 编排 Provider 和 Tool,是唯一能修改 History 的运行层。
- `cli` 和 `tui` 只发 UserAction、消费 Event。
- `eval` 调同一个 Agent runtime。

## 5. 核心协议

v1 只保留必要协议。

### 5.1 Message

```rust
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

pub enum ContentBlock {
    Text { text: String },
    Reasoning { text: String },
    ToolUse { call_id: String, name: String, input: serde_json::Value },
    ToolResult { call_id: String, content: String, status: ToolResultStatus },
}

pub enum ToolResultStatus {
    Success,
    Error,
    Rejected,
    Cancelled,
}

pub struct Message {
    pub id: String,
    pub role: Role,
    pub content: Vec<ContentBlock>,
    pub created_at: Timestamp,
}
```

v1 不支持 image。后续需要多模态再扩展。

不变量:

- raw history append-only。
- assistant 如果包含 `ToolUse`,必须跟对应 `ToolResult`。
- 已 commit 到 `messages.jsonl` 的消息不回写修改。
- 流式 token 不进入 `messages.jsonl`,只进入 `events.jsonl`。

### 5.2 Provider

```rust
pub trait Provider {
    fn capabilities(&self, model: &str) -> ModelCapabilities;
    async fn stream_chat(&self, request: ChatRequest) -> Result<ProviderStream, ProviderError>;
}
```

v1 只要求 streaming。`complete_once` 可作为 helper,不是核心协议必须项。

```rust
pub enum ProviderEvent {
    ReasoningDelta(String),
    TextDelta(String),
    ToolCallComplete(ToolCall),
    Usage(Usage),
    Done(StopReason),
}
```

DeepSeek 原始 SSE 只能在 `deepseek` crate 内解析。

### 5.3 ModelCapabilities

```rust
pub struct ModelCapabilities {
    pub supports_reasoning: bool,
    pub supports_tool_calls: bool,
    pub requires_reasoning_for_tool_turns: bool,
    pub supports_json_mode: bool,
    pub supports_prompt_cache_metrics: bool,
    pub max_context_tokens: usize,
    pub max_output_tokens: usize,
}
```

只保留 Agent runtime v1 会用到的能力。parallel tools、strict tools 等先作为 DeepSeek adapter 内部配置,等真正使用再上升为公共能力。

### 5.4 Tool

```rust
pub trait Tool {
    fn name(&self) -> &str;
    fn schema(&self) -> ToolSchema;
    fn risk(&self, input: &serde_json::Value) -> ToolRisk;
    async fn call(&self, input: serde_json::Value, ctx: ToolContext) -> Result<ToolOutput, ToolError>;
}
```

v1 Agent 可见工具:

- `Read`:读取指定文件的完整或部分内容。
- `Edit`:对现有文件进行精准、有针对性的修改。
- `Write`:创建或完全覆写文件内容。
- `Glob`:基于模式匹配快速查找文件和目录路径。
- `Grep`:在文件内容中通过正则或关键字搜索代码逻辑。
- `ListFiles`:列出特定路径下的文件和目录结构。
- `Bash`:在隔离或本地环境中执行 shell 命令行操作。

实现层可以继续拆成更小的工具,但 Provider tool spec 和 prompt 优先暴露上面的高层工具名。

当前实现到目标协议的映射:

| 当前实现 | 目标 Agent 工具 |
|---|---|
| `read_file` | `Read` |
| `apply_patch` | `Edit` |
| `write_file` | `Write` |
| `search` | `Glob` / `Grep` / `ListFiles` |
| `shell` | `Bash` |
| `run_tests` | `Bash` 的受控场景或内部 helper |
| `git_diff` | `Bash` / `Read` 的受控场景或内部 helper |

后置工具:

- `TaskCreate` / `TaskUpdate` / `TaskGet` / `TaskList`:任务状态和 TUI 展示成熟后。
- `Agent`:SubAgent 机制成熟后。
- `AskUserQuestion`:TUI、CLI、eval 下的交互语义统一后。

### 5.5 Event

`Event` 是 CLI/TUI/replay 共用事件协议。v1 的 `events.jsonl` 也是唯一事件账本。

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

后续评测需要更细审计时,再从 Event 派生独立 trajectory。

## 6. Agent Runtime

状态机保持线性:

```text
UserInput
  -> BuildPrompt
  -> StreamModel
  -> CommitAssistant
  -> MaybeExecuteTools
  -> CommitToolResults
  -> ContinueOrFinish
```

v1 必须保证:

- 没有 orphan tool call。
- 取消模型流时不写入半截 assistant message。
- 工具失败/拒绝/取消都写入 tool result。
- max turns 有硬限制。
- 所有稳定事件写入 `events.jsonl`。

### 6.1 Execution Loop

Flash Code v1 的 loop 是一个顺序 agent loop,不做多 agent 调度和并行工具调用:

```text
for turn in 1..=max_turns:
  build prompt
  stream provider response
  if response has no tool call:
    commit assistant
    finish
  commit assistant with tool calls
  resolve approval for each tool call
  execute approved tool calls sequentially
  commit tool results
  continue
```

loop 退出条件:

- 模型输出最终回答,且没有 tool call。
- 达到 `max_turns`。
- 用户取消。
- provider 返回不可恢复错误。
- 工具返回阻断性错误且模型无法继续。

v1 不支持:

- parallel tool calls。
- background tasks。
- 多 agent 协作调度。
- 自动长任务 daemon。

这些能力会改变事件顺序、权限模型和可复现性,等单 agent runtime 稳定后再引入。

### 6.2 Tool Registry

工具通过 registry 暴露给 Agent runtime:

```rust
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}
```

registry 只负责:

- 根据名称查找工具。
- 输出 provider 需要的 tool schema。
- 拒绝重复工具名。
- 标记工具风险等级。

registry 不负责:

- 审批决策。
- 执行 sandbox。
- 写 session 文件。
- 根据 provider 做特殊逻辑。

### 6.3 Permission Policy

权限分三层,保持简单:

1. Workspace boundary:工具默认只能读写当前 workspace。
2. Tool risk:每个工具根据输入计算风险。
3. Approval mode:`confirm`、`yolo`、`human` 决定是否执行。

默认策略:

| 风险 | confirm | yolo | human |
|---|---|---|---|
| Read | 自动允许 | 自动允许 | 只建议 |
| Write | 需要确认 | workspace 内允许 | 只建议 |
| Execute | 需要确认 | 非破坏命令可允许 | 只建议 |
| Network | 需要确认 | 需要确认 | 只建议 |
| Destructive | 需要确认 | 需要确认 | 只建议 |

`Destructive` 永远不能静默执行。`human` 模式下工具不会真实执行,只把建议命令或补丁写入事件流。

### 6.4 Skill

Skill 是“给 Agent 的本地能力说明和提示片段”,不是新的执行引擎。

v1 不实现动态 skill 系统。只保留架构位置:

```text
SkillSource
  -> selected skill instructions
  -> prompt projection
```

后续触发条件:

- 需要按项目类型注入固定工程规范。
- 需要把评测失败经验沉淀成可复用策略。
- 需要让用户维护本地工作流说明。

即使引入 Skill,也必须遵守:

- Skill 只能影响 prompt projection 和可用工具选择。
- Skill 不能绕过权限审批。
- Skill 不能直接写 session 文件。
- Skill 不能包含 secret 明文。

### 6.5 SubAgent

SubAgent 后置到 v1 之后。v1 只做单 Agent loop。

SubAgent 的合理用途是隔离复杂任务中的只读分析,例如:

- 代码库结构扫描。
- issue 复现路径分析。
- benchmark 失败归因。

不在 v1 引入的原因:

- 会增加消息存储结构复杂度。
- 会让 tool permission 和 event replay 更难保证。
- 对 Terminal-Bench / SWE-bench 初期 smoke 不必要。

如果后续引入,也应保持克制:

- SubAgent 默认只读。
- SubAgent 不能直接执行写操作。
- SubAgent 产出作为 parent Agent 的普通上下文输入。
- 所有 SubAgent 事件仍写入 parent session 的 `events.jsonl`,必要时增加 `parent_event_id`。

## 7. Prompt Projection

v1 Prompt Projection 只做三件事:

1. system prompt。
2. `messages.jsonl` 中的 committed messages。
3. 当前可用 tool specs。

DeepSeek 专项规则通过能力控制:

- 如果 `requires_reasoning_for_tool_turns=true`,保留发生 tool call 的 reasoning。
- 如果 `supports_prompt_cache_metrics=true`,记录 cache usage 到 Event。

v1 不实现复杂 compaction。上下文超限时先报错或做简单 tail 保留。等真实触发频繁后再设计 `compactions.jsonl`。

## 8. 本地存储

v1 强制文件:

```text
.flash/
  config.toml
  workspace.json
  sessions/
    session_xxx/
      session.json
      messages.jsonl
      events.jsonl
      artifacts/   # 按需
```

详见 [Flash Code 本地存储方案](storage.md)。

配置只支持两层:用户级 `~/.config/flash-code/config.toml` 和 workspace 级 `.flash/config.toml`。环境变量和 CLI 参数优先级更高。API key 不写入配置,只保存 `api_key_env`。

`.flash/` 是本地状态目录,默认忽略不提交。v1 不设计团队共享配置层;需要共享时先用文档说明,等需求稳定后再增加模板文件。

## 9. 安全与审批

v1 执行模式:

| 模式 | 行为 |
|---|---|
| `confirm` | 写操作和命令执行前确认 |
| `yolo` | workspace 内自动执行非破坏性操作 |
| `human` | 只建议命令,用户手动执行 |

工具风险:

- Read
- Write
- Execute
- Network
- Destructive

`Destructive` 永远需要显式确认。

## 10. 后续扩展边界

只有出现真实需求时再引入:

- 独立 `trajectory.jsonl`:接公开评测时。
- `compactions.jsonl`:上下文超限成为常见问题时。
- `index.json`:session 数量导致扫描变慢时。
- `sandbox` crate:工具执行边界复杂后。
- `telemetry` crate:报表和成本分析复杂后。
- 更多 Provider:DeepSeek 路径稳定后。

这份架构的重点是:先让核心 Agent 可运行、可恢复、可测试,再逐步扩展。
