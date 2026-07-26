# Flash Code 0.9 现代 Rust 技术栈重构计划

本文档定义 Flash Code 0.9 的完整技术栈目标、架构约束、分阶段任务和验收标准。0.9 的目标不是扩大产品形态,而是在保持现有 CLI/TUI/runtime 边界克制的前提下,把当前零第三方依赖原型演进为以 `tokio`、`clap`、`serde`、`reqwest`、`ratatui`、`crossterm` 为主的现代 Rust 实现。

---

## 1. 当前状态与重构动机

当前代码已经具备较清晰的 crate 边界:

- `flash-core`:协议、配置、workspace/session/storage、工具协议。
- `flash-provider`:Provider trait、ProviderEvent、错误和模型能力。
- `flash-deepseek`:DeepSeek SSE 解析和 HTTP 错误映射雏形。
- `flash-agent`:Agent runtime、tool loop、approval、prompt projection。
- `flash-tools`:内置工具实现。
- `flash-cli`:命令入口、配置加载、runtime 调用。
- `flash-tui`:当前 TUI 状态、输入循环、事件展示。
- `flash-eval`:评测入口,复用同一个 `AgentRuntime`。

但底层仍保留 0.1-0.8 阶段的零依赖实现:

| 当前实现 | 风险 | 0.9 目标 |
|---|---|---|
| 手写 JSON 转义、拼接、字段抽取 | JSON schema 漂移、逃逸错误、嵌套结构解析不可靠 | 用 `serde`/`serde_json` 统一协议、存储、Provider payload、评测结果 |
| 手写 `std::env::args()` 命令分发 | help/参数校验不可持续,子命令扩展容易破坏行为 | 用 `clap` derive 建模 CLI,保持命令集合克制 |
| 同步 Provider/runtime/tool 调用 | 真实 LLM SSE、TUI 输入、取消、事件展示难以自然并发 | runtime 以 `tokio` async 为中心 |
| `flash-deepseek` 只解析字符串,没有真实 HTTP client | `flash run` 仍依赖 smoke provider,无法真实流式调用模型 | 用 `reqwest` async stream 接入 DeepSeek |
| TUI 手写 ANSI 和 `stty` | 渲染闪烁、跨平台差、输入控制脆弱 | 用 `ratatui` 渲染、`crossterm` 终端控制 |

0.9 重构要保护现有稳定性:每个阶段都必须有明确改动范围、验收标准和提交点。完成一个阶段并提交后,才能进入下一阶段。

---

## 2. 技术栈目标

0.9 允许并优先使用以下主栈:

| 技术 | 使用位置 | 目标 |
|---|---|---|
| `serde` + `serde_json` | `core`、`provider`、`deepseek`、`tools`、`eval` | 所有持久化 JSON/JSONL 和 Provider payload 都由类型驱动序列化/反序列化 |
| `clap` | `cli` | 命令、参数、help、usage、错误提示由 derive struct/enum 定义 |
| `tokio` | `cli`、`agent`、`provider`、`deepseek`、`tui`、必要的 `tools` | runtime、Provider stream、TUI event loop 和异步 I/O 的统一运行时 |
| `reqwest` | `deepseek` | DeepSeek HTTP POST、错误状态映射、streaming body |
| `ratatui` | `tui` | layout、widgets、transcript、session list、input、approval 状态 |
| `crossterm` | `tui` | raw mode、alternate screen、keyboard events、terminal restore |

允许的辅助依赖必须服务于主栈,并在引入阶段说明原因。例如:

- `futures-util`:消费 `reqwest` byte stream 或 boxed stream。
- `thiserror`:库 crate 的错误类型 derive。
- `async-trait`:只有在不想手写 boxed future 且 trait object 需求明确时引入。

不为了“看起来现代”引入额外框架。0.9 不引入 Web UI、插件系统、复杂任务编排、SubAgent、遥测后端或新的交互式 CLI chat。

---

## 3. 架构原则

核心原则保持不变:

```text
Core Protocol: provider-agnostic
Provider Adapter: provider-specific
Runtime Policy: capability-aware
Product Default: DeepSeek-first
CLI/TUI: runtime callers and event views
```

含义:

- `flash-core` 不依赖 HTTP、DeepSeek、TUI、CLI 或 shell 实现。
- `flash-provider` 定义模型抽象,不执行工具,不读写 session。
- `flash-deepseek` 只负责 DeepSeek 请求/响应适配,不修改 history,不执行工具,不处理 UI。
- `flash-tools` 只执行工具能力,不调用模型,不直接写 session 文件。
- `flash-agent` 是核心 runtime,负责编排 Provider、Tool、approval、history、event log、取消、重试和 prompt projection。
- `flash-cli` 只是参数解析、配置加载、调用 runtime、输出基本结果。
- `flash-tui` 只是状态展示、输入采集、approval/cancel 用户动作转发、消费 runtime events。
- `flash-eval` 复用同一个 runtime,不建立评测专用 runtime,不绕过工具权限。

目标依赖方向:

```text
core
  ^
  |
provider       tools
  ^             ^
  |             |
deepseek        |
    \           |
     \          |
      runtime/agent
          ^
          |
       cli / tui / eval
```

`cli` 和 `tui` 可以依赖 `agent`,但 `agent` 不能依赖 `cli` 或 `tui`。TUI 相关类型不得泄漏到 runtime API。

---

## 4. Runtime 中心设计

0.9 的核心收敛点是 `flash-agent` runtime。CLI 和 TUI 的差异只能存在于输入输出层,不能复制 agent loop。

### 4.1 Runtime 负责

- 创建和加载 session。
- append-only 写入 `messages.jsonl` 和 `events.jsonl`。
- 调用 `ChatProvider` 并消费 streaming `ProviderEvent`。
- 将 Provider stream 映射为 `Event`。
- 维护 turn loop、tool loop、max turns、max tokens/bytes projection。
- 调用 `ToolRegistry`。
- 执行 approval policy。
- 处理取消。
- 处理 retryable Provider 错误。
- 输出 `AgentRun` 和事件流。

### 4.2 CLI 只负责

- 用 `clap` 解析 `flash run`、`flash tui`、`flash eval`、`flash replay`、`flash resume`、`flash doctor`、`flash init`。
- 读取配置和 CLI overrides。
- 构造 runtime 所需 provider、tool registry、options。
- 调用 runtime。
- 对 headless 命令打印最基本的 session/outcome/error。

CLI 不做:

- 不实现 tool loop。
- 不解析 Provider stream。
- 不直接写 history。
- 不包含交互式 chat 状态机。
- 不把 DeepSeek 特例写进通用命令分发。

### 4.3 TUI 只负责

- 用 `ratatui` 渲染 session list、transcript、input、status、approval prompt。
- 用 `crossterm` 读取键盘事件和管理终端模式。
- 将用户输入转换为 runtime run request。
- 将 approval/cancel 转为 runtime control signal。
- 消费 runtime event stream 并更新展示状态。

TUI 不做:

- 不执行工具。
- 不直接调用 DeepSeek。
- 不直接修改 messages/events。
- 不实现独立 prompt projection。
- 不为 UI 复制 runtime 状态机。

### 4.4 Runtime API 目标形态

API 可以在实施中微调,但目标语义如下:

```rust
pub struct AgentRuntime<P> {
    provider: P,
    tools: ToolRegistry,
    options: AgentOptions,
}

impl<P> AgentRuntime<P>
where
    P: ChatProvider,
{
    pub async fn run_task_with_controls<C, A, S>(
        &mut self,
        workspace_root: &Path,
        task: &str,
        controls: C,
        event_sink: S,
        approval: A,
    ) -> Result<AgentRun, AgentError>;
}
```

实际实现可以继续使用 generic callback、`tokio::sync::mpsc` 或 boxed sink。验收重点不是具体签名,而是:

- runtime 是唯一 agent loop。
- event 先持久化再通知上层。
- CLI/TUI/Eval 共享同一 runtime。
- 流式事件能被 headless 和 TUI 观察。

---

## 5. 协议与存储目标

### 5.1 Core protocol

`Message`、`ContentBlock`、`Event`、`SessionStatus`、`Outcome`、`ToolResultStatus`、`ToolRisk`、`PermissionDecision`、`ApprovalMode` 必须导出稳定的 `Serialize`/`Deserialize`。

`ContentBlock` 目标 schema:

```rust
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Reasoning { text: String },
    ToolUse {
        call_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        call_id: String,
        status: ToolResultStatus,
        content: String,
    },
}
```

`Event` 目标 schema:

```rust
#[serde(tag = "type", rename_all = "snake_case")]
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
    UsageRecorded { input_tokens: u64, output_tokens: u64 },
    Error { message: String },
    SessionFinished { outcome: Outcome },
}
```

### 5.2 Storage invariants

- `messages.jsonl` append-only。
- `events.jsonl` append-only。
- stream delta 只写 `events.jsonl`,不写 `messages.jsonl`。
- assistant message 只有收到 provider done 后才能 commit 到 `messages.jsonl`。
- tool result 必须跟对应 tool call 一起进入 history。
- session 仍绑定 workspace,跨 workspace resume 必须失败。
- 现有 golden JSONL 测试必须保留或有明确 schema 迁移说明。

### 5.3 JSON 迁移策略

先用 `serde_json` 替换手写 JSON 读写,再考虑把 storage I/O 改为 `tokio::fs`。这样能把 schema 风险和 async 风险拆开。

---

## 6. Provider 与 DeepSeek 目标

### 6.1 Provider abstraction

`ChatProvider` 必须表达 streaming 语义。目标可以是 callback async:

```rust
#[async_trait]
pub trait ChatProvider: Send + Sync {
    async fn chat(
        &mut self,
        request: ChatRequest,
        on_event: &mut (dyn FnMut(ProviderEvent) + Send),
    ) -> Result<(), ProviderError>;
}
```

也可以是 stream API:

```rust
pub trait ChatProvider: Send + Sync {
    fn stream_chat(
        &mut self,
        request: ChatRequest,
    ) -> ProviderStream<'_>;
}
```

实施时择一,但验收标准一致:

- Provider 层不写 session。
- ProviderEvent 顺序保持流式顺序。
- retryable 错误由 runtime 根据 `ProviderError::is_retryable()` 处理。
- `SmokeProvider` 保留为测试 provider,但生产 CLI 默认使用真实 provider。

### 6.2 DeepSeek adapter

`flash-deepseek` 负责:

- 从 config/env 获取 base URL、API key、model。
- 用 `reqwest::Client` 发送 `/chat/completions` 请求。
- 用 `serde` 构造请求体,包括 messages、tools、stream=true。
- 消费 SSE body stream。
- 用 typed structs 或 `serde_json::Value` 解析 nested delta、tool call、usage、finish_reason。
- 将 HTTP 400/401/402/422/429/5xx 映射为现有 `ProviderError` 语义。

`flash-deepseek` 不负责:

- 工具执行。
- approval。
- session/history 写入。
- TUI 展示。

---

## 7. TUI 目标

0.9 TUI 只追求稳定、可用、低复杂度:

- 双栏或响应式布局:session list + transcript。
- 底部 input。
- status line。
- pending approval 提示。
- live stream delta 展示。
- cancel/quit。
- resume 现有 session。

暂不做:

- 多 tab。
- command palette。
- rich markdown rendering。
- diff viewer。
- 鼠标交互。
- 复杂快捷键系统。
- TUI 内配置编辑器。

TUI 的实现必须使用:

- `ratatui` widgets/layout 渲染主要界面。
- `crossterm` raw mode、alternate screen、keyboard events、restore。

TUI 验收重点:

- 退出后终端模式恢复。
- 非 TTY/headless 场景仍可测试。
- TUI 不直接依赖 DeepSeek。
- TUI 不绕过 runtime。

---

## 8. CLI 目标

0.9 CLI 维持当前基本命令集合:

```text
flash                  -> TUI
flash tui              -> TUI explicit alias
flash run "<task>"     -> headless run
flash eval ...         -> benchmark/eval
flash replay ...       -> replay events
flash resume ...       -> resume/read existing session behavior
flash doctor           -> diagnostics
flash init             -> initialize workspace
flash --help           -> generated help
flash --version        -> generated version
```

CLI 用 `clap` derive 表达命令和参数。CLI 不新增大型 UX。所有真正执行任务的命令都通过 runtime。

---

## 9. 分阶段任务

每个阶段都必须满足:

- 改动范围清晰。
- 验收标准全部通过。
- `git status` 中只包含本阶段相关改动。
- 阶段结束创建一次提交。
- 提交后再进入下一阶段。

### Phase 0: 计划固化与基线确认

目标:

- 固化本 0.9 重构计划。
- 明确 runtime、CLI、TUI 的职责边界。
- 确认当前测试基线。

改动范围:

- 仅文档:本文件。
- 不修改 Rust 实现。

验收标准:

- 本文件完整描述技术栈目标、架构原则、runtime/CLI/TUI 关系。
- 本文件包含所有阶段性任务和每阶段验收标准。
- `cargo test --all-features` 通过。
- `git diff` 只包含计划文档改动。

提交:

- `docs: define 0.9 rust stack rearchitecture plan`

### Phase 1: Serde 协议和同步存储迁移

目标:

- 用 `serde`/`serde_json` 替换 core 中的手写 JSON 协议和 storage JSONL。
- 保持现有同步 runtime 行为不变。

改动范围:

- `crates/core/Cargo.toml`
- `crates/core/src/protocol.rs`
- `crates/core/src/storage.rs`
- 必要的 `crates/tui`/`crates/eval` JSON 读取适配
- golden fixtures

不做:

- 不引入 `tokio`。
- 不改 Provider trait async。
- 不改 TUI 渲染。
- 不接真实 DeepSeek。

验收标准:

- `Message`、`ContentBlock`、`Event`、status/risk/approval enum 使用 `Serialize`/`Deserialize`。
- core 不再手写 message/event JSON 拼接。
- core 不再依赖 `escape_json`/`find_json_string` 解析 session/message/event。
- `messages.jsonl` 和 `events.jsonl` schema 有 golden 测试覆盖。
- interrupted stream 不 commit partial assistant message 的测试仍通过。
- workspace mismatch 测试仍通过。
- `cargo fmt --check` 通过。
- `cargo test --all-features` 通过。
- `cargo clippy --all-targets --all-features -- -D warnings` 通过。

提交:

- `refactor(core): migrate protocol storage to serde`

### Phase 2: Clap CLI 迁移

目标:

- 用 `clap` derive 替换手写 CLI 参数解析。
- 保持命令集合克制,不扩大产品形态。

改动范围:

- `crates/cli/Cargo.toml`
- `crates/cli/src/main.rs`
- CLI help/usage 相关测试或 snapshot

不做:

- 不改 runtime loop。
- 不接真实 DeepSeek。
- 不重写 TUI。

验收标准:

- `flash --help` 由 clap 生成,包含现有命令。
- `flash --version` 正常输出版本。
- `flash run "<task>"` 仍调用同一个 `AgentRuntime`。
- `flash tui` 和无参数 `flash` 仍进入 TUI。
- `flash eval`、`flash replay`、`flash resume`、`flash doctor`、`flash init` 行为保持兼容。
- unknown command 和缺失参数由 clap 给出非零退出和 usage。
- CLI 不直接实现 agent loop。
- `cargo fmt --check` 通过。
- `cargo test --all-features` 通过。
- `cargo clippy --all-targets --all-features -- -D warnings` 通过。

提交:

- `refactor(cli): replace manual args with clap`

### Phase 3: Provider/Agent async runtime 迁移

目标:

- 引入 `tokio`,使 Provider 和 AgentRuntime 支持 async streaming。
- 保持 CLI/TUI/Eval 共享同一 runtime。

改动范围:

- `crates/provider`
- `crates/agent`
- `crates/cli`
- `crates/tui` 的 runtime adapter
- `crates/eval` 的 runtime adapter
- 必要的 tests

不做:

- 不实现真实 DeepSeek HTTP。
- 不重写 ratatui UI。
- 不扩大工具集。

验收标准:

- `ChatProvider` 支持 async streaming 语义。
- `AgentRuntime::run_task_with_controls` 或等价入口为 async。
- runtime 仍是唯一 tool loop/approval loop/history writer。
- event 仍先写入 `events.jsonl`,再通知 CLI/TUI/Eval。
- retryable provider error 仍最多重试并有测试覆盖。
- cancellation 仍不 commit partial assistant message。
- TUI 仍通过 adapter 调 runtime,不复制 runtime 逻辑。
- Eval 仍复用同一个 runtime,不建立评测专用 runtime。
- `SmokeProvider` async 化后继续覆盖现有 agent/eval 测试。
- `cargo fmt --check` 通过。
- `cargo test --all-features` 通过。
- `cargo clippy --all-targets --all-features -- -D warnings` 通过。

提交:

- `refactor(agent): make runtime and provider streaming async`

### Phase 4: DeepSeek reqwest streaming 接入

目标:

- 实现真实 DeepSeek provider。
- 让生产 `flash run` 默认可真实调用 DeepSeek API 并流式输出。

改动范围:

- `crates/deepseek/Cargo.toml`
- `crates/deepseek/src/lib.rs`
- `crates/cli` provider 构造
- `crates/core/src/config.rs` 的必要 serde/config 适配
- Provider request/response tests

不做:

- 不把 DeepSeek 逻辑写入 agent runtime。
- 不让 CLI/TUI 解析 SSE。
- 不要求无 API key 的测试访问网络。

验收标准:

- `DeepSeekProvider` 实现 `ChatProvider`。
- 请求体由 `serde` 构造,包括 messages、tools、model、stream。
- HTTP status 映射到 `ProviderError` 有测试覆盖。
- SSE parser 支持 reasoning delta、text delta、tool call、usage、done/tool_calls/max_tokens。
- parser 不用手写字符串字段抽取解析嵌套 JSON。
- 缺失 API key 时 `flash doctor` 能提示。
- 无 API key 的默认测试不访问真实网络。
- 有 API key 时手动验收:`DEEPSEEK_API_KEY=... flash run "say hello"` 能流式输出并创建 session。
- CLI/TUI 仍只构造 provider 并调用 runtime。
- `cargo fmt --check` 通过。
- `cargo test --all-features` 通过。
- `cargo clippy --all-targets --all-features -- -D warnings` 通过。

提交:

- `feat(deepseek): add reqwest streaming provider`

### Phase 5: Crossterm 终端控制迁移

目标:

- 用 `crossterm` 替换 `stty`、手写 raw mode 和基础键盘读取。
- 保持现有字符串 TUI 渲染,先降低终端控制风险。

改动范围:

- `crates/tui/Cargo.toml`
- `crates/tui/src/lib.rs`
- TUI terminal guard/input tests

不做:

- 不引入 ratatui layout。
- 不改变 runtime API。
- 不新增复杂快捷键。

验收标准:

- TUI 不再调用 `stty`。
- 使用 `crossterm::terminal::enable_raw_mode/disable_raw_mode`。
- 使用 alternate screen enter/leave。
- 退出、panic path 或 error path 后终端恢复逻辑有测试或可验证封装。
- keyboard event handling 支持输入、Enter、Backspace、Esc/Ctrl-C、q quit。
- 非 TTY/headless render-once 测试继续通过。
- `cargo fmt --check` 通过。
- `cargo test --all-features` 通过。
- `cargo clippy --all-targets --all-features -- -D warnings` 通过。

提交:

- `refactor(tui): replace stty terminal control with crossterm`

### Phase 6: Ratatui 渲染迁移

目标:

- 用 `ratatui` 替换手写 ANSI/string frame 渲染。
- TUI 保持基础可用,不做过度设计。

改动范围:

- `crates/tui/Cargo.toml`
- `crates/tui/src/lib.rs`
- golden/snapshot tests

不做:

- 不引入复杂组件系统。
- 不做 markdown/diff/rich text 大功能。
- 不改变 runtime 业务逻辑。

验收标准:

- TUI 主界面由 `ratatui` layout/widgets 渲染。
- 包含 session list、transcript、status、input、approval prompt。
- live stream event 能更新 transcript。
- resize 或小尺寸 viewport 不 panic。
- headless snapshot 测试覆盖关键状态。
- TUI 不直接写 session/history。
- TUI 不直接调用 provider/tool。
- `cargo fmt --check` 通过。
- `cargo test --all-features` 通过。
- `cargo clippy --all-targets --all-features -- -D warnings` 通过。

提交:

- `refactor(tui): render interface with ratatui`

### Phase 7: Tokio storage/tool I/O 收敛

目标:

- 在 runtime async 基础稳定后,将必要的 storage 和工具 I/O 迁移到 async。
- 避免在 async runtime 中长时间阻塞。

改动范围:

- `crates/core/src/storage.rs`
- `crates/tools` 中 shell/file 相关 I/O
- `crates/agent` 调用点

不做:

- 不为了 async 改变 JSON schema。
- 不重写工具协议。
- 不引入 sandbox 新 crate。

验收标准:

- session/message/event 写入在 runtime async path 中使用 async I/O 或明确隔离阻塞操作。
- shell execution 使用 `tokio::process` 或明确 `spawn_blocking` 边界。
- file read/write 工具不阻塞核心 event loop。
- 事件顺序和 append-only 不变量不变。
- 大输出 artifact 行为不变。
- tool risk/approval 行为不变。
- `cargo fmt --check` 通过。
- `cargo test --all-features` 通过。
- `cargo clippy --all-targets --all-features -- -D warnings` 通过。

提交:

- `refactor(runtime): move storage and tool io onto tokio`

### Phase 8: 全量回归、验收和清理

目标:

- 验证 0.9 技术栈重构完成。
- 清理过时零依赖 helper。
- 更新文档和验收记录。

改动范围:

- docs
- tests
- dead code cleanup
- scripts/check.sh 如有必要

验收标准:

- `rg "escape_json|find_json_string|stty|std::env::args"` 不再命中生产代码中的旧实现。
- `Cargo.toml` 中明确使用主栈依赖:`tokio`、`clap`、`serde`、`serde_json`、`reqwest`、`ratatui`、`crossterm`。
- `scripts/check.sh` 或 CI gate 包含 fmt/test/clippy。
- `cargo fmt --check` 通过。
- `cargo test --all-features` 通过。
- `cargo clippy --all-targets --all-features -- -D warnings` 通过。
- 如安装 `cargo-nextest`,则 `cargo nextest run --all-features` 通过。
- `flash --help` 为 clap help。
- `flash doctor` 能报告 provider/model/API key 状态。
- `flash run` 经 runtime 调用真实 provider 或在无 API key 场景给出清晰错误。
- `flash tui` 经 runtime 调用任务,不复制 runtime 逻辑。
- `flash eval` 复用 runtime,不存在评测专用 runtime。
- 文档记录 0.9 完成状态和已知后续事项。

提交:

- `chore: complete 0.9 rust stack migration validation`

---

## 10. 总体验收标准

0.9 完成必须同时满足:

1. 主栈落地:`tokio`、`clap`、`serde`、`serde_json`、`reqwest`、`ratatui`、`crossterm` 出现在对应 crate,并用于生产路径。
2. runtime 中心:CLI、TUI、Eval 均调用同一个 `AgentRuntime`,不存在独立 agent loop。
3. CLI 克制:CLI 只做参数解析、配置、runtime 调用和基础输出。
4. TUI 克制:TUI 只做输入、展示、approval/cancel 转发和 event consumption。
5. Provider 分层正确:DeepSeek HTTP/SSE 只在 `flash-deepseek`,runtime 只消费 provider abstraction。
6. Storage 稳定:messages/events append-only,stream delta 不污染 messages,partial assistant 不 commit。
7. Tool 权限稳定:approval policy、risk、tool result status 行为不因 async/TUI 改造退化。
8. 测试绿色:`cargo fmt --check`、`cargo test --all-features`、`cargo clippy --all-targets --all-features -- -D warnings` 全部通过。
9. 手动验收通过:`flash --help`、`flash doctor`、`flash run`、`flash tui`、`flash replay` 的基本路径可用。
10. 每个阶段都有独立提交,提交历史能反映阶段边界。

---

## 10.1 0.9 完成状态

截至 2026-07-26,0.9 Rust 主栈迁移已完成:

- `serde`/`serde_json` 已用于 core 协议、storage JSONL、DeepSeek payload、TUI/eval JSON 读取辅助。
- `tokio` 已用于 CLI runtime、provider/runtime async path、storage/tool blocking boundary。
- `clap` 已接管 CLI 命令解析和 help/version。
- `reqwest` 已用于 DeepSeek streaming provider,DeepSeek HTTP/SSE 逻辑限定在 `flash-deepseek`。
- `crossterm` 已接管 TUI raw mode、alternate screen 和 keyboard event。
- `ratatui` 已接管 TUI 主界面 layout/widgets 渲染。
- CLI、TUI、Eval 均继续复用 `AgentRuntime`,没有新增独立 agent loop。
- 每个阶段均已独立提交,提交历史可追踪阶段边界。

已知后续事项:

- Provider callback 当前仍是同步 callback;runtime 对 live delta 保持“同步写 event 后 notify”以维持流式可见性。若后续要完全 async 化这一路径,应将 provider event sink 升级为 async sink 或 stream API。
- Eval JSON 报告仍保留轻量字符串模板,但 escaping/field 读取已由 `serde_json` helper 承担;若报告 schema 继续扩展,可再迁移为完整 typed report structs。
- 0.9 不包含复杂 TUI UX、markdown/diff rendering、插件面板或 sandbox 新 crate。

---

## 11. 不合理修改清单

以下修改不属于 0.9 合理范围,除非另开计划:

- 让 `cli` 或 `tui` 直接执行工具。
- 让 `cli` 或 `tui` 直接解析 DeepSeek SSE。
- 在 `agent` 中写 `if provider == "deepseek"`。
- 为 eval 创建专用 runtime 或绕过 permission policy。
- 一次性重写所有 crate 而没有阶段提交。
- 在 TUI 阶段引入复杂 UX,例如 command palette、diff viewer、插件面板。
- 改变 session append-only 语义。
- 为了 async 改掉现有工具安全边界。
- 在没有测试覆盖的情况下改变 JSONL schema。
- 引入与主栈无关的大型框架。

---

## 12. 推荐执行顺序

按阶段顺序执行:

```text
Phase 0 plan
  -> commit
Phase 1 serde core
  -> commit
Phase 2 clap cli
  -> commit
Phase 3 tokio async runtime
  -> commit
Phase 4 reqwest deepseek
  -> commit
Phase 5 crossterm terminal
  -> commit
Phase 6 ratatui render
  -> commit
Phase 7 async storage/tools
  -> commit
Phase 8 full validation
  -> commit
```

这个顺序故意把 schema、CLI、runtime、provider、terminal control、rendering、I/O 收敛拆开,避免在同一个提交里同时改变数据格式、并发模型和 UI 行为。
