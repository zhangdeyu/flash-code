# Flash Code 渐进式技术方案

本文档定义 Flash Code 的产品路线和每个阶段的可验收标准。项目原则是克制:架构、协议和存储只满足当前阶段需求,不提前实现大而全的平台。

## 1. 产品定位

Flash Code 是一款 DeepSeek-first 的 Rust 终端 Coding Agent。

- 项目名:`flash-code`
- 命令:`flash`
- 默认入口:`flash` 进入 TUI
- 显式 TUI 入口:`flash tui`
- 自动化入口:headless CLI
- 不规划 Web UI
- 不把 interactive CLI chat 作为主产品界面

核心体验:

- 在本地 workspace 中理解、修改、测试代码。
- 支持 DeepSeek streaming、reasoning、tool calls。
- 支持可恢复 session。
- 支持 replay。
- 后续支持 Terminal-Bench 和 SWE-bench Verified。

## 2. 核心架构判断

### 2.1 TUI 后置

TUI 从 0.4 开始。0.1 到 0.3 先完成 headless runtime,因为:

- runtime 比 UI 更影响长期正确性。
- 评测和 CI 必须依赖 headless CLI。
- TUI 只应该消费 Event,不应该驱动核心设计。

阶段性入口约定:

- 0.1 到 0.3:`flash` 可以显示帮助,提示使用 `flash run` / `flash doctor`。
- 0.4 起:`flash` 默认进入 TUI。
- `flash tui` 保留为显式别名,方便脚本、调试和文档表达。
- 带子命令时进入对应 CLI 模式,例如 `flash run`、`flash eval`、`flash replay`。

### 2.2 DeepSeek-first,不是 DeepSeek-only

```text
Core Protocol: provider-agnostic
Provider Adapter: provider-specific
Runtime Policy: capability-aware
Product Default: DeepSeek-first
```

v1 只实现 DeepSeek Provider,但核心协议不写死 DeepSeek 字段。

### 2.3 存储保持简单

v1 只强制:

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

不强制:

- `index.json`
- `trajectory.jsonl`
- `compactions.jsonl`
- `snapshots/`
- 多 profile 配置
- 独立 provider 配置文件

## 3. v1 技术栈

必选:

- Rust workspace
- `tokio`
- `reqwest`
- `serde`,`serde_json`
- `thiserror`
- `anyhow` 仅用于 binary 边界
- `clap`
- `tracing`

0.2 起按需:

- `portable-pty` 或 `tokio::process`
- `ignore` / `walkdir`
- `wiremock`
- `nextest`

0.4 TUI:

- `ratatui`
- `crossterm`

不急于引入:

- full telemetry system
- plugin system
- complex project index
- separate sandbox crate
- database

## 4. v1 Workspace

先用 6 个核心 crate:

```text
crates/
  core/
  provider/
  deepseek/
  agent/
  tools/
  cli/
```

后续:

- 0.4:`tui`
- 0.6:`eval`
- 需要时再拆 `sandbox`、`telemetry`、`xtask`

## 5. v1 协议

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
```

v1 不支持 image、summary role、复杂 nested content。需要时再加。

### 5.2 Event

`Event` 是 CLI、TUI、replay 共用协议。v1 存到 `events.jsonl`。

必须包含:

- session start/finish
- user message appended
- model request started
- reasoning delta
- assistant delta
- assistant message completed
- tool requested/started/output/finished
- approval required/resolved
- usage recorded
- error

### 5.3 Provider

v1 Provider 只要求:

- streaming chat
- capability
- provider error mapping

非流式 completion、parallel tool calls、strict tools 先不作为核心接口。

## 6. DeepSeek v1 集成

v1 必须支持:

- `deepseek-v4-flash`
- `deepseek-v4-pro`
- streaming
- reasoning delta
- text delta
- tool calls
- usage
- prompt cache hit/miss metrics
- 400/401/402/422/429/500/503 错误映射

v1 可选:

- JSON mode
- strict tool schema
- Anthropic-compatible endpoint

## 7. 渐进式路线与验收标准

### 0.1 协议、存储与 CLI 骨架

目标:项目能启动,协议能落盘,session 能创建和 replay。

交付:

- Cargo workspace。
- `core`,`provider`,`cli`。
- `flash --version`。
- `flash init` 或首次运行自动初始化 `.flash/`。
- `flash doctor`。
- 用户级和 workspace 级 config 读取。
- `.flash/workspace.json`。
- `session.json`。
- `messages.jsonl`。
- `events.jsonl`。
- Message/Event 基础类型。
- Provider trait。
- Tool trait。
- Tool registry 雏形。
- Permission policy 类型定义。

验收标准:

- `cargo fmt --check` 通过。
- `cargo clippy --all-targets --all-features -- -D warnings` 通过。
- `cargo test` 通过。
- 在 git repo 内运行 `flash doctor` 能识别 workspace root。
- `.flash/` 默认在 `.gitignore` 中忽略。
- 配置优先级为 default -> user config -> workspace config -> env -> CLI args。
- `DEEPSEEK_API_KEY` 缺失时 `flash doctor` 明确提示,但不打印 secret。
- `flash init` 或首次运行能创建 `.flash/workspace.json`。
- 新建 session 后生成 `.flash/sessions/session_xxx/session.json`。
- user message 能写入 `messages.jsonl`。
- event 能写入 `events.jsonl`。
- `flash replay .flash/sessions/session_xxx/events.jsonl` 能打印事件时间线。
- 在另一个目录执行 `flash resume <session_id>` 会拒绝并提示 workspace 不匹配。
- 同名 tool 注册会失败并返回明确错误。
- permission policy 能根据 Read/Write/Execute/Network/Destructive 给出 approve/reject/ask 的决策。

### 0.2 DeepSeek Headless Agent MVP

目标:没有 TUI 也能完成最小 Agent 闭环。

交付:

- `deepseek` crate。
- DeepSeek streaming parser。
- `agent` crate。
- `flash run "<task>"`。
- reasoning/text 流式输出。
- Agent 可见工具雏形:`Read`、`ListFiles`、`Bash`。
- 内部兼容实现:`read_file`、`search`、`shell`。
- confirm/yolo/human 三种模式的最小实现。
- 工具 stdout/stderr 写入 events,长输出写入 artifacts。
- 顺序 Agent loop:模型 -> tool approval -> tool execution -> tool result -> 继续模型。

验收标准:

- mock DeepSeek streaming 测试覆盖 reasoning delta、text delta、tool call、usage、done。
- `flash run "list files"` 能触发 `ListFiles` 或兼容 search tool 并结束。
- tool call 成功、失败、拒绝、取消都会产生 tool result。
- 每轮 loop 都受 `max_turns` 限制,超限后 session 明确失败并写入 event。
- v1 不执行 parallel tool calls;多个 tool call 按稳定顺序执行并写入 event。
- 模型流中取消不会写入半截 assistant message 到 `messages.jsonl`。
- shell 命令超时后能终止并写入 error tool result。
- `events.jsonl` 能 replay 出模型输出和工具输出。
- 429/500/503 有有限重试;401/402 不重试。

### 0.3 代码修改能力

目标:Agent 能在真实 workspace 中做小型代码修改并验证。

交付:

- Agent 可见工具:`Read`、`Edit`、`Write`、`Glob`、`Grep`、`ListFiles`、`Bash`。
- 内部兼容实现:`apply_patch`、`write_file`、`git_diff`、`run_tests`。
- workspace 写入边界校验。
- 简单 prompt projection/token budget。
- 简单上下文过长处理:报错或保留最近消息,不做复杂 compaction。

验收标准:

- `flash run "fix failing tests"` 能执行读文件、改文件、跑测试的闭环。
- 所有文件写入限制在 workspace root 内。
- destructive 命令即使在 yolo 下也必须确认。
- `human` 模式下不会真实执行 shell/write/test,只输出建议操作。
- 修改后能输出最终 diff 摘要。
- 失败测试输出过长时写入 artifact,message/event 只保留摘要和路径。
- 同一任务失败时可通过 replay 看到关键工具步骤。

### 0.4 TUI MVP

目标:TUI 成为主交互界面,但不改变 runtime。

交付:

- `tui` crate。
- transcript 视图。
- reasoning 展开/折叠。
- tool output 视图。
- approval UI。
- input box。
- cancel。
- resume 当前 workspace session。
- `flash` 默认进入 TUI。
- `flash tui` 作为显式别名保留。

验收标准:

- 无参数执行 `flash` 会进入 TUI,不会进入 headless run。
- `flash tui` 能列出当前 workspace 的 sessions,列表可通过扫描 `session.json` 得到,不依赖 `index.json`。
- TUI 能从 `events.jsonl` 重建 transcript。
- TUI 执行任务和 `flash run` 使用同一个 Agent runtime。
- TUI 退出后终端 raw mode/alternate screen 正常恢复。
- 窄屏下主要文本不重叠。
- TUI snapshot 覆盖普通输出、reasoning、tool output、approval、error。

### 0.5 内部测试体系

目标:核心功能具备防退化能力。

交付:

- unit tests。
- integration tests。
- mock DeepSeek server。
- golden event log tests。
- basic TUI snapshot tests。
- CI 脚本。

验收标准:

- `cargo test --all-features` 通过。
- `cargo nextest run --all-features` 通过。
- Provider 错误映射覆盖 400/401/402/422/429/500/503。
- History 测试覆盖 tool use/result 配对。
- Event replay golden 测试能捕捉事件顺序变化。
- storage 测试覆盖 workspace resume 拒绝、interrupted turn 不进 messages。

### 0.6 Terminal-Bench Smoke

目标:接入主公开基准的小样本,验证评测路径。

交付:

- `eval` crate 或 `flash eval` 模块。
- Terminal-Bench smoke adapter。
- 独立 eval run 目录。
- 结果 JSON。
- 失败原因初步分类。

验收标准:

- 能跑固定 Terminal-Bench smoke subset。
- 每个任务调用同一个 Agent runtime。
- 每个任务保存 `events.jsonl`。
- report 包含 task id、pass/fail、耗时、命令数、token usage、失败原因。
- 失败任务可 replay。
- 不存在评测专用 prompt/runtime 分支绕过正常 Agent。

### 0.6.x Skill 机制评估点

目标:只在真实需要时引入 Skill,避免过早把 prompt 系统复杂化。

触发条件:

- 多个项目反复需要相同工程规范注入。
- Terminal-Bench/SWE-bench 失败分析证明缺少可复用策略层。
- 用户需要维护本地工作流说明。

验收标准:

- Skill 只能影响 prompt projection 或可用工具选择。
- Skill 不能绕过 permission policy。
- Skill 不能直接写 `messages.jsonl` / `events.jsonl`。
- Skill 文件中不得保存 secret 明文。

### 0.7 SWE-bench Verified Smoke

目标:验证真实 issue 修复路径。

交付:

- SWE-bench Verified smoke runner。
- repo checkout。
- issue prompt builder。
- patch collector。
- grader bridge。

验收标准:

- 能跑 10 个固定 SWE-bench Verified task。
- 每个 task 生成 patch 或明确失败原因。
- report 记录 resolved/unresolved。
- 失败可归类为定位失败、patch 失败、测试失败、环境失败、超时。
- 所有任务仍通过同一个 Agent runtime。

### 0.7.x SubAgent / Task / AskUser 评估点

目标:只有单 Agent loop、基础 event 状态和 approval 已经不足时,再评估更复杂的交互协议。

触发条件:

- 大仓库代码定位明显拖慢主 loop。
- 评测失败主要来自上下文收集不足。
- 需要把分析任务和执行任务隔离。
- TUI 中长期任务需要结构化 todo,单纯 events 难以表达进度。
- 模型频繁需要向用户询问歧义,approval 不能表达问题类型。

验收标准:

- SubAgent 默认只读。
- SubAgent 不能直接执行写操作。
- SubAgent 结果作为 parent Agent 的普通上下文输入。
- SubAgent 事件可从 parent session 的 `events.jsonl` replay。
- `TaskCreate` / `TaskUpdate` / `TaskGet` / `TaskList` 只能更新 session 内任务状态,不能替代 `messages.jsonl` 或 `events.jsonl`。
- `Task*` 状态必须能从 `events.jsonl` replay 重建。
- `AskUserQuestion` 在 TUI、CLI、headless eval 下都有明确行为。
- headless eval 下 `AskUserQuestion` 默认失败或使用预置答案,不能挂起无限等待。
- 所有新增工具仍受 permission policy 和 workspace guard 约束。

### 0.8 回归评测

目标:形成小而稳定的防退化机制。

交付:

- 私有 regression task set。
- Terminal-Bench fixed subset。
- SWE-bench Verified fixed subset。
- 趋势 JSON/Markdown 报告。

验收标准:

- 一条命令能跑 smoke regression。
- 报告能对比本次与上次 pass rate。
- 报告能列出新增失败任务。
- 报告能链接到对应 replay 文件。
- release 前必须有一次 smoke regression 结果。

### 1.0 日常可用

目标:成为可日常使用、可恢复、可评测的 DeepSeek TUI Coding Agent。

验收标准:

- 用户可用 `flash` 完成常见代码修改任务。
- 用户可用 `flash tui` 显式进入同一个 TUI。
- 用户可用 `flash run` 做单次自动化任务。
- session 可 resume、可 replay。
- workspace resume 边界可靠。
- 内部测试稳定。
- Terminal-Bench 和 SWE-bench smoke 都有报告。
- 所有失败都有可追溯 event log。

## 8. 命令设计

入口规则:

- `flash`:默认进入 TUI。0.4 前临时显示帮助。
- `flash <subcommand>`:进入 CLI 模式。
- `flash tui`:显式进入 TUI,等价于无参数 `flash`。

0.1 到 0.3 必须:

```bash
flash init
flash doctor
flash run "fix the failing tests"
flash replay .flash/sessions/session_xxx/events.jsonl
flash resume session_xxx
```

0.4:

```bash
flash
flash tui
```

0.6:

```bash
flash eval terminal-bench --subset smoke
```

0.7:

```bash
flash eval swe-bench --subset verified --limit 10
```

可选调试:

```bash
flash chat
```

## 9. 文档关系

- [渐进式迭代计划](iteration_plan.md):每个版本的目标、交付、验收和退出条件。
- [架构方案](architecture.md):模块边界和核心协议。
- [数据流转方案](data_flow.md):streaming、tool、event、replay 的流转。
- [本地存储方案](storage.md):v1 session 文件结构。
- [Agent 工具协议](tool_protocol.md):暴露给 Agent 的稳定工具名和权限语义。

后续需要时再拆:

- `docs/deepseek.md`
- `docs/testing.md`
- `docs/evaluation.md`
- `docs/tui.md`
