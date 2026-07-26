# Flash Code Agent 完整改进方向与任务计划（不含 TUI）

> 基线日期：2026-07-26
> 输入：`docs/codex_pi_arch_review.md`、当前仓库实现与既有 0.9/1.0 计划
> 范围：Agent Runtime、Context、Tool、权限与沙箱、Session、Provider、MCP/扩展、评测与工程质量
> 明确排除：TUI 布局、渲染、键位、Markdown 展示和其他 UI 体验工作

## 1. 结论

Flash Code 当前已经不是架构评审文档中描述的早期原型。1.0 收口已完成结构化 Tool Calling、completion/finalize 状态机、ToolTurn 不变量、byte-safe SSE、attempt 级重试、端到端取消、macOS 网络沙箱、continuation session、原子存储和崩溃恢复。

下一阶段不应再次重做这些能力，也不应立即复制 Codex 的 99-crate 或 Pi 的完整插件体系。最合理的主线是：

1. 先拆开目前仍然过大的核心模块，建立稳定的 Runtime 控制面和扩展边界。
2. 用真正的 compaction 替代简单 history projection，补齐长任务能力。
3. 将 Tool 执行升级为异步、可流式、可拦截、可安全并行的执行系统。
4. 将权限从风险枚举升级为可解释、可持久化、按作用域管理的策略引擎。
5. 在统一 Tool/Policy/Hook 边界之上接入 MCP，而不是让 MCP 绕过核心安全链路。
6. 再建设 thread fork、steering/follow-up、后台运行和多 Agent；这些能力必须复用同一套 Session、Event、Tool 与权限契约。
7. 用真实任务评测和可观测性约束每个阶段，避免只增加架构复杂度。

目标不是“功能最多”，而是形成一个能够长时间运行、可恢复、可扩展、可审计，并能被 CLI、未来 daemon 或其他前端稳定调用的 Agent 内核。

## 2. 对原评审文档的代码校准

| 原文判断 | 当前代码事实 | 规划处理 |
|---|---|---|
| `agent/src/lib.rs` 3517 行 | 当前为约 3778 行；此外 `core/storage.rs` 约 1630 行、`tools/lib.rs` 约 1714 行 | 模块拆分仍是 P0，范围扩大到 Agent、Storage、Tools |
| 无 Thread/Fork | 已有 `parent_session_id`、祖先 history 和 `flash continue`；但只能按完整 session continuation，不能从任意消息节点 fork | 保留现有 continuation，后续升级为 thread graph/message-boundary fork |
| Provider/runtime/tool 同步 | Provider 和 Runtime 已 async，Provider 使用 bounded channel；Tool trait 仍同步并由 `spawn_blocking` 包装 | 保留 Provider 异步 transport，重构其消息/事件契约；Tool 执行面升级为原生 async |
| 无 cancellation | token 已贯穿 Runtime、Provider、ToolContext 和 Bash 进程组 | 不重复建设；补审批异步化、并行子任务取消语义 |
| 无明确 completion/finalize | `TurnCompletion`、唯一 finalize 和终态一致性已实现 | 作为不变量继续保留 |
| SSE/重试可能重复 delta | byte-safe SSE、request/attempt 事件、发布 delta 后禁止透明重试已实现 | 不重复建设；补充 tracing 与故障注入 |
| event sequence 为 O(n²) | Session 已维护内存 sequence counter | 不重复建设；补多 writer/daemon 场景的单写者约束 |
| Bash 无可靠网络边界 | macOS 已使用 `sandbox-exec` 网络拒绝，其他平台严格离线 allowlist | 后续升级为 capability-aware sandbox，不把字符串识别当最终策略 |
| 无 compaction | 当前只有按完整 turn 的 `project_history` 截断 | 属于下一阶段 P0 |
| 无 before/after Tool hook | 确实缺失 | 属于下一阶段 P0 |
| 无并行 Tool | 确实按稳定顺序串行执行 | 在 effect/capability 分类之后实现 |
| 无持久化审批规则 | 确实缺失 | 属于安全能力 P1 |
| 无 MCP/扩展 | 确实缺失 | 在 Tool/Hook/Policy 边界稳定后实现 |

### 2.1 Codex 与 Pi 源码核验后的关键取舍

本计划又直接核验了：

- `/Users/derek/Project/github/openai/codex`
- `/Users/derek/Project/github/earendil-works/pi`

源码给出的重要补充：

1. Codex 的 Tool 并行不是通用依赖分析器，而是 Tool 通过 `supports_parallel_tool_calls()` 声明能力，Runtime 用读写锁让“可并行 Tool 共享、串行 Tool 独占”。Pi 同样先采用全局 parallel/sequential 模式，并允许单个 Tool 声明 `executionMode = "sequential"`；并发完成后仍按原 ToolCall 顺序生成 ToolResult。
2. 因此 Flash Code 首版并行应先实现“显式 execution mode + 有序提交”，快速获得确定收益；路径/effect 冲突分析作为第二层增强，不能阻塞首版。
3. Pi compaction 不只在完整 user turn 边界切分。单个 turn 本身超过保留预算时，会在不留下孤立 ToolResult 的合法边界切分，并分别总结历史和当前 turn prefix。Flash Code 必须覆盖这种极端情况。
4. Codex compaction 会重新注入 canonical initial context/WorldState，并区分 pre-turn/manual 与 mid-turn 的插入位置；还尽量维持前缀稳定以提高 prompt cache 命中。Flash Code 的 compaction 也必须显式处理 context reinjection 和 cache stability。
5. Pi 的 Hook 明确区分 observer 与会改变语义的 handler，并为不同事件定义不同合并规则；Codex 的 extension contributors 也将 lifecycle observation、MCP contribution 等职责分开。Flash Code 不应把所有扩展逻辑塞入一个万能 callback。
6. Codex MCP 包含并发启动、required server、连接复用、tool catalog revision/cache、tool filter、timeout、stdio/streamable HTTP、OAuth、resources 和 elicitation。第一版可以缩小范围，但数据模型必须为 catalog 动态变化和连接级元数据留出位置。
7. Codex 的 Tool catalog cache 特意删除 live connection annotations，因为这些 annotations 会影响审批和并行判断，不能跨连接复用。Flash Code 必须把远端 annotation 当不可信、连接作用域输入。
8. Codex Tool 有 `Direct/Deferred/DirectModelOnly/Hidden` 暴露模式。MCP 或大量外部 Tool 上线后，全部 schema 每轮注入会快速消耗 context；延迟发现和 schema 大小预算应纳入 MCP 阶段。
9. Codex steering 接口携带 expected turn id，并拒绝不可 steering 的 review/compact turn。Flash Code 也必须防止迟到的 steering 被误投递到下一 turn。
10. Pi 的 Project Trust 是实际核心能力，不只是安全理念；但 Pi 的 TypeScript extension 明确拥有用户完整系统权限。Flash Code 可学习其 trust 加载顺序，不应照搬同进程任意代码执行边界。
11. Codex 明确分开 provider `ResponseEvent`、provider-neutral `ResponseItem`、运行期 `TurnItem/EventMsg` 和 app-server notification。Added/Delta/Done 通过 item id 关联，模型 response completed 与 Agent turn completed 不是同一终态。
12. Pi 的 `Message` 是紧凑的 provider-neutral union，`AgentMessage` 可承载 custom message，并只在 LLM 调用边界执行 `convertToLlm`；其 `AssistantMessageEvent` 对 text/thinking/toolcall 都提供 start/delta/end 和唯一 done/error terminal。
13. Pi 每个 delta 携带完整 partial message 对 TypeScript/UI 很方便，但在 Rust 中会放大 clone；Flash Code 应学习其事件语义，不复制其快照成本。Pi 的 partial tool JSON 仅作为 scratch 并在完成时清除，这一隔离必须保留。
14. 因此消息设计采用“Pi 式紧凑 canonical transcript + Codex 式 item lifecycle/correlation + Rust reducer 强不变量”：Provider wire、Model stream、Agent message 和 Runtime event 四层不可混用。

### 2.2 自研与第三方依赖边界

Flash Code 不应把“架构自主”理解为“所有基础设施都自己实现”。正确边界是：

- 自研产品语义和安全组合：Agent 状态机、Tool 生命周期、EffectSet、权限合并、Approval/ExecutionGrant、Session/Event、compaction 不变量。
- 复用成熟机制：异步运行时、协议编解码、JSON Schema、路径匹配、操作系统安全原语、SQLite、系统凭据库、Secret 内存包装、tracing。
- 自研薄适配层：将第三方库封装在 Flash Code 自己的 trait 和领域类型后，避免库类型扩散到 `flash-core` 公共协议。
- 安全能力采用“库/系统原语 + 我们的策略 + 端到端验证”，不能因为依赖成熟就省略 capability probe、fail-closed、绕过测试和审计。

依赖选择默认采用 **Codex-aligned** 原则：

- Codex 已使用且适合 Flash Code 的库，默认采用相同 crate、相同 major/minor 和相近的最小 feature 集。
- Flash Code 不需要定时机械同步 Codex 的每个 patch version；升级以本仓库 lockfile、MSRV 和 contract tests 为准。
- 只有在功能缺失、复杂度明显不匹配、license/MSRV/native 风险不可接受或 Flash Code 已有实现更可靠时才偏离，并在 ADR/PR 中记录原因。
- Pi 是 TypeScript 项目，主要用于学习协议和 Agent 语义，不以复刻 npm 依赖为目标；Rust 机制库优先与 Codex 对齐。

以当前本地 Codex 源码为初始基线：

| 类别 | Codex 基线 | Flash Code 策略 |
|---|---|---|
| HTTP/SSE | `reqwest 0.12`、`bytes 1.10`、`eventsource-stream 0.2` | 保持 `reqwest 0.12`；评估用 `eventsource-stream` 替换自研 SSE framing，供应商 JSON 映射仍自研 |
| Async/stream | `tokio 1`、`tokio-util 0.7`、`futures 0.3` | 对齐；用 `tokio-util::CancellationToken` 替代重复取消原语 |
| 错误 | `thiserror 2` | library crate 使用 typed error；CLI 边界才允许通用错误包装 |
| Schema | `schemars 0.8` | schema 生成对齐；`jsonschema` 仅在注册/调用时确需本地验证时作为有记录的补充依赖 |
| 搜索/规则 | `ignore 0.4`、`globset 0.4`、`url 2` | 对齐 |
| Sandbox | `landlock 0.4`、`seccompiler 0.5`，Linux 使用 bubblewrap | 对齐库与总体组合，不复制 Codex 私有实现 |
| MCP | `rmcp 1.8`，关闭默认 feature | 对齐稳定 major/minor 和按需 feature，不追随 3.x prerelease |
| Secret | `keyring 3.6`、`zeroize 1.8` | 对齐；暂不默认增加 Codex 未使用的 `secrecy`，除非 spike 证明能减少泄漏面 |
| State | `sqlx 0.9` + bundled SQLite | 真正需要 SQLite 时优先对齐；当前 JSONL 阶段不提前引入 |
| Observability | `tracing 0.1`、`tracing-subscriber 0.3` | 对齐；OpenTelemetry 按需引入 |
| 常用协议类型 | `serde 1`、`serde_json 1`、`uuid 1` | 对齐 |

引入依赖前使用统一准入检查：

1. 最近仍有维护，release、issue 和安全响应不是长期停滞状态。
2. 有真实生产用户或被 Codex、Rust 生态等成熟项目验证，而不只看下载量。
3. license、MSRV、native dependency、二进制体积和跨平台范围可接受。
4. 能关闭无关默认 feature，公共 API 可被薄适配层隔离。
5. 有测试/fuzz/安全说明；安全关键库要检查 RustSec/GitHub advisory。
6. 先做 spike 和失败测试，再锁定稳定版本；不直接追随 prerelease。

建议选型矩阵：

| 能力 | 决策 | 优先采用 | Flash Code 仍需负责 |
|---|---|---|---|
| Async、取消、stream | 直接复用 | `tokio`、`tokio-util::sync::CancellationToken`、`futures` | turn/tool 取消语义、背压上限、提交顺序 |
| JSON/Tool Schema | 复用生成与验证 | 与 Codex 对齐的 `serde`、`schemars`；按需补 `jsonschema` | ToolDescriptor、错误映射、rewrite 后复验和 schema 大小限制 |
| 文件遍历与规则匹配 | 直接复用 | `ignore`、`globset`、`url` | workspace guard、canonicalization、symlink 与规则作用域 |
| Linux filesystem/process sandbox | 组合复用 | `bubblewrap` 作为首选隔离进程，`landlock` 作为能力允许时的内核限制/后备，`seccompiler` 生成 seccomp-BPF | SandboxPlan、安装探测、参数生成、降级策略、跨层测试 |
| macOS sandbox | 复用系统设施 | 保留 `/usr/bin/sandbox-exec`/Seatbelt backend | profile 生成、能力探测、平台限制说明；不存在可直接替代完整策略的通用 Rust crate |
| Windows sandbox | 复用 OS API | `windows` crate 调用 restricted token、Job Object、AppContainer 等原语 | 组合成 backend、兼容性探测和安全测试；没有可靠的一站式跨平台 crate 时不伪造支持 |
| MCP | 封装采用 | 与 Codex 对齐的 `rmcp 1.8` stable line | namespace、连接管理、catalog cache、quota、Policy/Hook 适配；不直接暴露 SDK 类型 |
| Session/Thread 索引 | 需求触发后采用 | SQLite；异步路径优先 `sqlx`，极简同步组件可选 `rusqlite` | event schema、事务边界、migration、单 writer 和恢复语义 |
| Credential/Secret | 组合复用 | 与 Codex 对齐的 `keyring`、`zeroize` | credential precedence、日志/事件 redaction；`zeroize` 不能消除已经复制到其他缓冲区的 secret |
| Tracing/Telemetry | 直接复用 | `tracing`、`tracing-subscriber`；需要导出时再接 OpenTelemetry | span schema、敏感字段策略、默认关闭远程 telemetry |
| Patch/Diff | 封装采用 | 评估 `diffy` 等成熟 parser/apply 库 | workspace guard、文件 fingerprint、原子写、权限和审计 |
| Shell/规则解析 | 组合复用 | argv 优先；`shlex`/shell parser、必要时 `tree-sitter-bash` | exec policy 语义、解释器与 shell wrapper 绕过识别 |
| 依赖审计 | 直接复用 | `cargo-deny`、`cargo-audit`、Dependabot/Renovate 等 | allow/deny policy、升级验证和安全响应 SLA |

明确不应外包给通用库的部分：

- `Deny > Ask > Allow`、Project Trust 和 PermissionProfile 的产品语义。
- approval key、持久化规则和 `ExecutionGrant` 的安全不变量。
- ToolResult 有序提交、崩溃恢复和副作用幂等性。
- compaction 保留哪些信息以及 ToolTurn 合法边界。
- MCP/扩展是否可信；协议 SDK 只解决通信，不解决权限。

依赖治理要求：

- 第三方依赖集中声明在 workspace，关闭不需要的默认 feature，提交并使用 `Cargo.lock`。
- 新增依赖先检查 Codex workspace 是否已有同类选择；默认记录 Codex crate/version/feature 对照。
- 安全关键能力通过内部 trait 隔离，例如 `SandboxBackend`、`SchemaValidator`、`CredentialStore`、`McpTransport`。
- 每个 backend 必须报告 `Available/Degraded/Unavailable` 和实际 capability；请求的限制无法兑现时默认拒绝，禁止静默裸跑。
- 任何替换自研代码的依赖 PR 都必须同时提交：选型记录、最小 spike、失败模式、许可证/安全检查、性能或复杂度收益。
- 不以“减少代码量”为唯一理由换库；如果现有实现短小、契约稳定且第三方依赖带来更大供应链或 native 风险，可以保留自研。

## 3. 目标架构

```text
CLI / Eval / future daemon clients
                │
                ▼
         AgentService API
  start / continue / fork / steer / follow-up / cancel
                │
                ▼
          SessionRunner
  ├── TurnEngine
  │   ├── ContextManager
  │   ├── ProviderExecutor
  │   └── CompletionPolicy
  ├── ToolScheduler
  │   ├── HookChain
  │   ├── PolicyEngine
  │   ├── SandboxManager
  │   └── ToolExecutor
  ├── SessionStore
  │   ├── messages / events
  │   ├── checkpoints
  │   └── compactions
  └── EventBus / RuntimeCommandBus
                │
       ┌────────┴────────┐
       ▼                 ▼
 Built-in Tools      MCP / external tools
```

边界要求：

- `flash-core` 只放稳定协议、配置值对象和存储接口，不放 Agent 流程。
- `flash-provider` 只表达 Provider 能力和事件，不知道 Tool 如何执行。
- `flash-agent` 负责编排，不直接实现 Bash、MCP transport 或 Provider DTO。
- 所有内置、MCP 和未来扩展 Tool 都必须经过同一个 Hook、Policy、Sandbox 和审计链路。
- UI 不进入 Runtime 类型；Runtime 只暴露 command/event API。
- 不采用 Rust 动态库 ABI 作为插件边界，优先使用进程外协议，避免 ABI、崩溃隔离和供应链问题。

## 4. P0：先完成内核解耦

### A0.1 拆分 `flash-agent`

建议模块：

```text
crates/agent/src/
  lib.rs
  runtime.rs
  service.rs
  session_runner.rs
  turn.rs
  provider_attempt.rs
  tool_scheduler.rs
  context.rs
  hooks.rs
  control.rs
  finalize.rs
  error.rs
```

任务：

- 将公开 API 与内部状态机分开，`lib.rs` 只做 re-export。
- 把测试从 2000+ 行内联测试拆到模块测试和 `tests/` 集成测试。
- 删除 `execute_tool_call` 中 Allow/Ask/Deny 三条重复执行路径，统一为“决策 → 审批 → 执行 → 提交”管线。
- 将 `SmokeProvider` 移到 test-support 或独立 fixture 模块，避免生产 runtime 文件承载评测模拟。
- 为模块依赖写一条 compile-time/API 约束测试，禁止 Storage/Tool 反向依赖 Agent。

验收：

- 生产模块原则上不超过 500 行；状态机模块有理由时可放宽到 800 行。
- 不改变现有 session/event/message schema 和 CLI 行为。
- 现有质量门禁全部通过。

### A0.2 拆分 `flash-core::storage` 与 `flash-tools`

建议拆分：

```text
core/src/storage/
  mod.rs
  session.rs
  message_log.rs
  event_log.rs
  recovery.rs
  atomic.rs
  limits.rs

tools/src/
  lib.rs
  registry.rs
  fs/{read,write,edit,glob,grep,list}.rs
  exec/{mod,process,output,sandbox,policy_hint}.rs
 path_guard.rs
```

存储选型：

- 当前 JSONL append log、原子 metadata 和恢复测试已经工作，M1/M2 不为“用了数据库”而迁移。
- 当 thread graph 查询、daemon 多客户端或索引性能成为真实需求时，再以 SQLite 作为派生索引/事务状态；异步 runtime 优先评估 `sqlx`，如果只放在专用 blocking worker 中可评估 `rusqlite`。
- message/event 原始记录的领域 schema、唯一 finalize 和恢复语义继续由 Flash Code 定义，SQLite 不能成为绕过这些不变量的第二真相源。

验收：

- 原子写、JSONL 尾部修复、唯一 finalize、workspace/symlink guard 的现有测试保持不变。
- Bash 进程、输出捕获、artifact 和 sandbox 可分别测试。

### A0.3 引入异步 Runtime 控制面

当前 callback、同步 `ApprovalController` 和多个 `run_task_*` 重载会阻碍 steering、后台运行和 daemon。

目标 API：

```rust
pub enum RuntimeCommand {
    Cancel,
    Approve { call_id: String, decision: ApprovalDecision },
    Steer { message: String },
    FollowUp { message: String },
}

pub struct RunHandle {
    pub session_id: String,
    pub commands: Sender<RuntimeCommand>,
    pub events: Receiver<EventEnvelope>,
    pub completion: JoinHandle<Result<AgentRun, AgentError>>,
}
```

任务：

- `AgentService::start` 返回 `RunHandle`，运行过程不借用 UI/CLI callback。
- 分发策略：主路径继续使用 `AgentRuntime<P>` 静态分发；`RunHandle` 保持非泛型，泛型 `P` 被捕获在 spawned future 中、返回的 `JoinHandle` 已擦除具体 Provider 类型。当前 `ChatProvider::chat(&mut self, ...)` 配合 `#[async_trait(?Send)]` 使 future 非 `Send`，无法直接交给多线程 `tokio::spawn`，必须先将 Provider future 改为 `Send`。`start` 消费一个 run-owned runtime/provider，或通过 `ProviderFactory` 为每次 run 创建实例。只有运行时确需异构 Provider registry 时，才在 factory 边界使用 `Arc<dyn ChatProvider>`；不让 `dyn ChatProvider` 扩散进 Agent 热路径。
- 审批改为异步 command/response，并支持 cancel 与 timeout。
- 在行为契约测试通过后，用与 Codex 一致的 `tokio-util::sync::CancellationToken` 替换自研取消 token，统一 parent/child cancellation。
- 现有 headless `run_task` 保留为薄封装，内部消费同一个 `RunHandle`。
- Event envelope 加入 `session_id`、单调 sequence、可选 `turn_id/request_id`。

验收：

- CLI headless 行为不变。
- Provider stream、审批等待和 Tool 执行期间都能接收 RuntimeCommand。
- 慢 consumer 不导致无限内存；明确 bounded channel 和背压策略。

### A0.4 规范化消息与四层流式事件

当前模型已经能安全处理 byte-split SSE、文本/思考 delta、完整 ToolCall、usage 和终止原因，但类型边界仍偏扁平：

- `ProviderEvent` 只有 `ReasoningDelta/TextDelta/ToolCallComplete/Usage/Done`，无法表达 item start/end、content index、交错内容块、tool arguments delta 和 reasoning summary/raw 的区别。
- `Message { role, content }` 允许构造语义上非法的组合，例如 user message 携带 ToolUse；ToolResult 的模型可见内容也不是独立强类型。
- provider delta 先实时写 Event，之后又从收集的 `Vec<ProviderEvent>` 二次聚合成 message，容易产生重复 clone、双重终态和“live 与 committed 不一致”。
- `Done(StopReason)` 与 `ChatProvider::chat() -> Result` 同时承担终止语义，调用方必须推断哪个才是最终结果。
- `AssistantDelta` 只关联 request/attempt，没有稳定 item id/content index，未来多个 assistant/reasoning/tool item 交错时无法可靠归并。

结合 Codex 与 Pi，采用四层模型：

```text
Provider wire event
  ↓ provider-private decoder
ModelStreamEvent
  ↓ validated stream reducer
Canonical AgentMessage / completed ModelOutputItem
  ↓ runtime projection
RuntimeEvent<ItemStarted / ItemDelta / ItemCompleted>
```

四层分阶段建设，不一次建到完整 item lifecycle：第一阶段 Provider wire 保持私有；第二阶段只实现 DeepSeek 所需的最小 `ModelStreamEvent`；第三阶段落地 role-specific canonical message；第四阶段的完整 item 交错、content index、raw/summary reasoning 区分等扩展，等第二 Provider（A6.2）证明需要再补。当前设计已预留这些位置，但 M1 不强制一次实现。

#### 第一层：Provider wire 类型保持私有

- DeepSeek/OpenAI/Anthropic 等 SSE JSON struct 只存在于各 provider crate。
- wire event 不写 session、不直接发给 CLI，也不进入 `flash-core` 公共协议。
- framing 优先评估与 Codex 一致的 `bytes` + `eventsource-stream`；供应商事件映射和上限检查仍由 adapter 负责。
- adapter 必须把 provider 特有 finish reason、usage、reasoning、tool call 和 response id 映射为统一语义；未知字段可忽略，未知关键终态必须报错。

#### 第二层：Provider-neutral `ModelStreamEvent`

学习 Codex `ResponseEvent` 的 item 生命周期，同时采用 Pi 的显式 `start/delta/end` 内容语义：

```rust
pub type ModelEventStream =
    Pin<Box<dyn Stream<Item = Result<ModelStreamEvent, ProviderError>> + Send>>;

pub enum ModelStreamEvent {
    ResponseStarted {
        response_id: Option<String>,
    },
    ItemStarted {
        item_id: ItemId,
        kind: ModelItemKind,
    },
    TextDelta {
        item_id: ItemId,
        content_index: u32,
        delta: String,
    },
    ReasoningDelta {
        item_id: ItemId,
        content_index: u32,
        channel: ReasoningChannel,
        delta: String,
    },
    ToolArgumentsDelta {
        item_id: ItemId,
        call_id: ToolCallId,
        delta: String,
    },
    ItemCompleted {
        item: ModelOutputItem,
    },
    Usage {
        usage: ModelUsage,
    },
    ResponseCompleted {
        response_id: Option<String>,
        stop_reason: StopReason,
    },
}
```

Provider trait 调整为“建立 stream”和“消费 stream”分离：

```rust
pub trait ChatProvider: Send + Sync {
    fn stream(
        &self,
        request: ChatRequest,
    ) -> BoxFuture<'_, Result<ModelEventStream, ProviderError>>;
}
```

Provider 需要运行时异构注册时，在这一 API 边界使用 `Arc<dyn ChatProvider>` 和 `BoxFuture`；已知具体类型的内部热路径仍优先静态分发。这样可与 Codex 使用的 `futures` 生态对齐，也避免为了单个 trait 继续扩散 `async-trait`。

语义约束：

- 建立连接/请求失败由外层 `Result` 返回；建立后的失败由 stream item 的 `Err(ProviderError)` 返回。
- 每个成功响应必须恰好一个 `ResponseCompleted`；EOF、channel close 或 `[DONE]` 前缺少真实 provider terminal event 都是错误。
- `ResponseCompleted` 只代表一次模型响应结束，不等于 Agent turn 已完成；Tool 调用后可能继续采样。
- `ItemStarted → Delta* → ItemCompleted` 以 `item_id` 关联，允许多个内容块和未来 provider 的交错事件。
- usage 表示本次 upstream response 的精确值；session 累计值由 Runtime 单独计算，不能混为同一类型。
- stop reason 为 closed enum + `Unknown(String)`；`MaxTokens` 下出现的 ToolCall 一律不可执行。

#### 第三层：Canonical AgentMessage

学习 Pi 在 Agent 内部使用 provider-neutral `UserMessage | AssistantMessage | ToolResultMessage`，但在 Rust 中使用 role-specific enum 让非法组合不可表示：

```rust
pub struct MessageEnvelope {
    pub id: MessageId,
    pub created_at: Timestamp,
    pub message: AgentMessage,
}

pub enum AgentMessage {
    System(SystemMessage),
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
    Custom(CustomMessage),
}

pub struct AssistantMessage {
    pub content: Vec<AssistantContent>,
    pub stop_reason: StopReason,
    pub usage: Option<ModelUsage>,
    pub provider: ProviderIdentity,
    pub response_id: Option<String>,
}

pub enum AssistantContent {
    Text {
        text: String,
        phase: Option<MessagePhase>,
    },
    Reasoning {
        summary: Vec<String>,
        raw: Vec<String>,
        opaque_state: Option<ProviderOpaqueState>,
    },
    ToolCall {
        call_id: ToolCallId,
        name: String,
        arguments: serde_json::Value,
    },
}

pub struct ToolResultMessage {
    pub call_id: ToolCallId,
    pub tool_name: String,
    pub status: ToolResultStatus,
    pub content: Vec<ToolResultContent>,
}
```

规则：

- system/developer instructions 与普通 user content 保持类型区分；Provider adapter 在请求边界做 role/content 投影。
- 像 Pi 的 `convertToLlm` 一样，custom/compaction/memory message 先保留 Agent 语义，只在调用模型前转换；不能为了某个 Provider 污染持久消息类型。
- provider-specific response id、reasoning signature/encrypted state 只在确实需要跨 turn round-trip 时，以 namespaced opaque 类型保存；日志默认脱敏，换 provider 时不得透传。
- partial tool JSON 是 reducer scratch state，不属于 `AgentMessage`，不能写入 session。只有 terminal item 的完整参数经过严格 JSON 解析和 schema 校验后才能形成 ToolCall。
- provider error/cancel 是运行结果，不伪装成成功的 assistant transcript；是否保存诊断 message 由 Runtime 的失败策略明确决定。

#### 第四层：Runtime item/event 协议

学习 Codex 的 `TurnItem` 与 `ItemStarted/Delta/ItemCompleted`，替换按 UI 字符串分类的扁平 delta：

```rust
pub struct EventEnvelope {
    pub session_id: SessionId,
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub request_id: Option<RequestId>,
    pub attempt: Option<u32>,
    pub sequence: u64,
    pub emitted_at: Timestamp,
    pub event: RuntimeEvent,
}

pub enum RuntimeEvent {
    TurnStarted,
    ItemStarted { item: TurnItem },
    ItemDelta { item_id: ItemId, delta: ItemDelta },
    ItemCompleted { item: TurnItem },
    PermissionEvaluated { /* ... */ },
    ApprovalRequired { /* ... */ },
    ToolOutputDelta { /* ... */ },
    TurnCompleted { outcome: TurnOutcome },
    TurnFailed { error: AgentErrorInfo },
}

pub enum ItemDelta {
    AgentText { content_index: u32, delta: String },
    ReasoningSummary { content_index: u32, delta: String },
    ReasoningRaw { content_index: u32, delta: String },
    ToolArgumentsPreview { delta: String },
}
```

`TurnItem` 是面向 Runtime client 的稳定投影，可以包含 AgentMessage、Reasoning、CommandExecution、FileChange、MCP ToolCall、Compaction 等；它既不是 provider wire `ResponseItem`，也不是 session transcript 的替代品。

持久化和背压：

- completed `AgentMessage`/ToolResult 是模型上下文真相源，`ItemCompleted` 是运行历史投影真相源；delta 绝不能成为恢复时重建最终消息的唯一来源。
- delta 默认作为 live event；如果为了 attach/replay 写盘，应进入可裁剪的 stream trace，而不是 durable transcript，并在 ItemCompleted 后允许压缩。
- Provider → Runtime 使用 bounded `mpsc` 且不丢事件，通过背压限制内存。
- Runtime → 多 client fan-out 可以只对相同 `(turn_id, item_id, delta kind)` 的相邻文本 delta 做有界合并；Started、Completed、approval、error、terminal 事件永不丢弃。
- 不照搬 Pi 每个 delta 携带完整 partial message 的做法。Rust reducer 内部持有唯一可变 accumulator，事件只携带 delta；只在 ItemCompleted、checkpoint 或显式 snapshot 时生成 owned clone。
- 大型 completed item 必要时 `Box` 化或使用 `Arc<str>/Arc<[T]>`，但必须先用 benchmark 和 `large_enum_variant` 检查证明收益。

流式 reducer 必须验证以下状态语法：

```text
ResponseStarted?
  → { ItemStarted | Delta | ItemCompleted | Usage }*
  → ResponseCompleted
```

- reducer 为每个 item 维护独立状态，允许 provider 合法交错多个 active item；ResponseCompleted 时 active item 集合必须为空。
- delta 引用未知 item、重复 ItemStarted/ItemCompleted、terminal 后继续发事件、未完成 ToolCall、重复 terminal 均返回 typed `StreamProtocolError`。
- provider 未提供 item id 时由 adapter 在 ItemStarted 时生成稳定 id，并在该响应内维护映射；不能每个 delta 重新生成。
- reasoning summary 与 raw reasoning 分开，且带 content/summary index；raw reasoning 只有在 provider 明确提供且配置允许时才保存/发布，不能在 adapter 中混成普通文本。
- retry 继续沿用当前安全规则：对外发布任意不可撤销 delta 后不透明重试；未发布时可以按 attempt 级规则重试。

验收：

- DeepSeek adapter contract fixture 覆盖 text、reasoning、多个 ToolCall、usage、max tokens、cancel、unknown finish reason 和 early EOF。
- 使用 Codex 风格 `OutputItemAdded/Delta/OutputItemDone/Completed` 与 Pi 风格 `start/text_start/thinking_start/toolcall_start/delta/end/done` fixture，证明都能归一为同一消息。
- partial tool arguments 即使能被宽松 parser 临时解析，也不能执行或进入 durable message。
- 每个 completed assistant message 等于其 start/delta/end reducer 结果；live delta 拼接与 replay 的 ItemCompleted 一致。
- 序列化 snapshot 固定 tag、必填字段和兼容默认值；协议演进新增字段优先 optional/default，破坏性变更必须提升 schema version。
- property/state-machine tests 覆盖乱序、重复、截断、超大 delta、UTF-8 分片、慢 consumer 和取消竞态。

### A0.5 分层、短小、能力感知的 System Prompt

当前 `prompts/system_default.md` 已经覆盖身份、Tool 使用、输出风格和安全提醒，Runtime 再追加 OS、shell、cwd 与 Tool 名称。它比空白 prompt 好，但仍有四个结构性问题：

- 所有长期行为规则集中在一个静态 Markdown，Tool 实际增删后只有名称变化，没有同步选择指南；
- 固定 Safety 文案与真正的 `PermissionPolicy/SandboxPlan` 分离，未来不同权限模式下容易出现“prompt 说不能、Runtime 实际允许”或相反；
- system message 只在新 session 创建时写入，continuation、工具变化、cwd/world state 变化后可能继承陈旧快照；
- 缺少 project instructions、来源/优先级、prompt version、size budget 和 deterministic snapshot，无法解释某次模型实际收到了什么。

从 Codex 与 Pi 中分别学习：

| 参考 | 应学习 | 不应照搬 |
|---|---|---|
| Codex | base instructions、permissions、AGENTS、skills、world state 分层；动态片段带来源；静态前缀稳定 | 当前面向多产品、多模式、多 Agent 的超长完整行为手册 |
| Pi | 默认 prompt 短小；只列实际启用 Tool；Tool 自带 snippet/guideline；context files 清晰分隔 | 把所有 project context 和 extension rewrite 都拼成一个无法区分信任边界的大字符串 |

#### Prompt 不是安全边界

- System Prompt 只解释 Runtime 已经决定的能力，不能授予权限。
- `PermissionPolicy/PermissionProfile/ProjectTrust/SandboxCapability` 的真实值由 Runtime 生成，不能由模板、仓库文件或模型文本声明。
- Prompt 中即使出现“允许执行”，PolicyEngine 仍可 Deny；Prompt 中的安全提醒也不能替代 PathGuard、approval、sandbox 和 secret redaction。
- `AskUserQuestion` 不负责权限审批；prompt 必须明确两者不同，但真正隔离由协议保证。

#### 首版只保留五类片段

```text
1. BaseAgentContract       固定、短小、版本化
2. ActiveToolGuidance      根据本轮实际 Tool catalog 生成
3. RuntimePolicyContext    根据真实权限/沙箱能力生成
4. ProjectInstructions     按目录作用域加载；M4 后叠加 ProjectTrust gate
5. TurnWorldState          cwd/os/shell/git/task/context budget 等动态事实
```

不为每个未来功能预建 fragment 类型。Skills、MCP、multi-agent 等能力上线时再贡献自己的有界片段，并复用同一个 `PromptFragment` 接口。

#### 所有权与分阶段交付

A0.5 只拥有 **prompt 组装机制**：fragment 数据结构、稳定顺序、版本、digest、预算接口、Provider role 投影和 snapshot。它不拥有各片段背后的发现或决策语义：

- A1.3 生产 `ProjectInstructions` 和可随 producer 扩展的 `TurnWorldState`，定义 `AGENTS.md` 作用域、来源、冲突、刷新与裁剪策略；
- A2.x 的 `ToolRegistry/ToolSpec` 生产 `ActiveToolGuidance` 输入；
- A2.4/A3.x 的 canonical sandbox/policy/trust 类型生产 `RuntimePolicyContext` 输入；
- A7 的 durable `TaskState` 落地后，A1.3 才把任务摘要加入 `TurnWorldState`；
- A6 只在 Provider contract fixture 证明需要时生产 model-specific overlay。

未就绪的 producer 不创建空 fragment、占位文案或未来类型的 stub，而是从 `PreparedPrompt.fragments` 中省略；这既避免模型把占位内容当成事实，也使 M1 不依赖 M3/M4/A7 的类型。

按里程碑交付：

| 阶段 | A0.5 可验收范围 | 后续接入 |
|---|---|---|
| M1 | fragment framework、`BaseAgentContract`、基于现有 `ToolRegistry` 的最小 `ActiveToolGuidance`、OS/shell/cwd 最小 WorldState、稳定排序/version/digest/snapshot | 不要求 `PermissionProfile/ProjectTrust/SandboxCapability/TaskState` 存在 |
| M2 | A1.3 接入有作用域的 `ProjectInstructions` 与 git/context 等当前可得的扩展 WorldState；A1.1 提供 fragment budget/cropping policy | 组装器本身不复制发现和裁剪逻辑 |
| M3 | A2.x 的正式 `ToolSpec.prompt_snippet` 和 sandbox capability producer 接入 | 只有实际注册且模型可见的 Tool 进入 guidance |
| M4 | A3.x 的 `ApprovalPolicy/PermissionProfile/ProjectTrust` 与 sandbox capability 共同生成 `RuntimePolicyContext` | 权限模式变化验收从此阶段开始 |
| A7/M7 | durable `TaskState` 摘要按需进入 WorldState | 不改变已有 fragment 协议 |

建议接口：

```rust
pub enum PromptFragmentKind {
    Base,
    ToolGuidance,
    RuntimePolicy,
    ProjectInstructions,
    WorldState,
}

pub struct PromptFragment {
    pub kind: PromptFragmentKind,
    pub source: String,
    pub priority: u16,
    pub content: String,
    pub digest: String,
}

pub struct PreparedPrompt {
    pub version: String,
    pub fragments: Vec<PromptFragment>,
    pub estimated_tokens: u64,
}
```

这里不需要复杂 type-state 或插件框架；固定 enum、稳定排序、严格 size limit 已足够。`PreparedPrompt` 是诊断/测试结构，Provider adapter 再把片段投影为其支持的 system/developer/user roles。

#### 固定 BaseAgentContract

基础 prompt 只表达跨模型、跨权限模式都成立的行为，建议控制在约 250-500 英文 token：

```text
You are Flash Code, a coding agent working in the user's repository.

Work until the requested task is resolved or a real blocker requires user input.
Inspect relevant files before making assumptions. Make focused changes and avoid
unrelated refactors. Use the available tools instead of inventing results.

After changing code, run the smallest relevant verification supported by the
repository. Never claim that a command, test, or edit succeeded unless its result
was observed. Preserve user changes and do not expose secrets.

Ask a focused question only when missing information would materially change the
result or when the runtime requires the user's decision.

In the final response, lead with the outcome, mention verification performed, and
state any remaining limitation or unverified assumption.
```

固定层不应包含：

- 当前 Tool 名称、参数或不存在的能力；
- OS、cwd、git branch、时间和 token budget；
- 某个权限 preset 的行为；
- Codex/Pi 自身文档路径；
- 未来 daemon、多 Agent、MCP、skills 的操作说明；
- 与 DeepSeek/OpenAI 某个模型 quirks 绑定的规则。

#### ActiveToolGuidance

Tool schema 是参数真相源，System Prompt 不重复完整 schema，只提供一行职责和必要的跨 Tool 选择规则。内容必须从**当前实际注册且模型可见**的 Tool descriptor 生成：

```text
Available tools:
- Read: inspect file contents.
- Glob: find files by path pattern.
- Grep: search text or regular expressions in project files.
- Edit: replace one unique segment in one existing file.
- Write: create a file or intentionally replace its entire contents.
- Bash: run a command and return its bounded output.

Tool selection:
- Use Glob for paths, Grep for contents, and Read for exact context.
- Prefer Edit for a focused existing-file change; use Write for new files or a
  deliberate complete rewrite.
- Use Bash for builds, tests, version control inspection, and operations not
  covered by a safer native tool.
```

`Agent/AskUserQuestion/TaskCreate/TaskUpdate/TaskList` 只有实际启用时才加入；Tool 未注册时不得在 prompt 中提到。M1 可用现有 Tool 描述生成最小 guidance；M3 接入正式 `ToolSpec.prompt_snippet`。跨 Tool guideline 仍由 core 维护并做 snapshot，避免扩展任意改写基础行为。

#### RuntimePolicyContext

学习 Codex 根据当前模式生成 permissions instructions，但 Flash Code 首版只呈现事实：

```text
Runtime policy:
- Approval policy: on-request.
- Permission profile: workspace-write.
- Filesystem writes are limited to: <workspace>.
- Network access: unavailable.
- Commands requiring unavailable capabilities will be denied.
```

要求：

- 该片段由 A3.x 的 canonical policy/trust 类型和 A2.4 的 sandbox capability 共同生产；M4 前不作为 A0.5 的完成前置；
- 只从 canonical policy/sandbox capability 渲染；
- 不把审批规则全文、命令 allowlist 或安全实现细节放进 prompt；
- capability 不可兑现时明确 unavailable，不能用模糊措辞诱导模型反复尝试；
- `FullAccess` 只说明实际能力，不鼓励扩大任务范围。

#### ProjectInstructions

A0.5 只负责渲染 A1.3 已发现、排序、限额并标注来源的 typed inputs，不重复定义目录发现、信任、冲突或刷新规则。M1 没有该 producer 时省略本片段；A1.3 是这组内容策略的唯一所有者。

#### TurnWorldState

A0.5 只负责稳定渲染调用方提供的 typed facts。M1 仅注入现有 Runtime 可直接证明的 workspace/cwd、OS 和 shell；git、context/compaction、sandbox capability 与 TaskState 等内容由 A1.3 在相应 producer 上线后加入。内容选择、刷新和脱敏策略也只在 A1.3 定义。

#### Prompt 顺序、缓存与 compaction

稳定顺序：

```text
BaseAgentContract
→ ActiveToolGuidance
→ RuntimePolicyContext
→ ProjectInstructions（浅到深）
→ TurnWorldState
→ UserMessage
```

- Base 和未变化的 Tool/Policy/Project 片段保持 byte-identical，保护 prompt cache。
- 每个 fragment 有独立 token/byte budget；`ProjectInstructions/WorldState` 的优先级与裁剪算法由 A1.1 `ContextBudget` 和 A1.3 内容策略共同定义，A0.5 只执行确定性的预算结果。Base 与 RuntimePolicy 不裁剪。
- Compaction 只总结 conversation history，不总结或改写 Base、Policy、当前 ProjectInstructions 和当前 WorldState；压缩后重新注入 canonical fragments。
- Session 记录 prompt version、fragment source/digest 和 model profile，不默认记录可能含敏感内容的完整动态正文。

#### Model-specific overlay

首版以 DeepSeek 行为为准，但不复制一份 DeepSeek 专用大 prompt。只有 contract fixture 证明必要时才增加小型、版本化 overlay，例如 tool-call 参数稳定性或 reasoning channel 规则；第二 Provider 上线后验证这些规则应该留在共享 base、Provider adapter 还是 model profile。

#### 验收与评测

机制不变量：

- 相同 fragment 输入生成 byte-identical fragment content 和 digest；整份 prompt 只有在完整 fragment 集合及顺序都相同时才要求 byte-identical。
- 未注册 Tool、未加载 project source 不出现在 prompt；capability 只能来自 canonical producer，可明确呈现 unavailable，但不能编造未授予能力。
- M1 snapshot 只验收 Base、现有 Tool guidance、最小 WorldState、顺序/version/digest；缺少 producer 的 fragment 必须不存在而非空占位。
- M2 增加 project instruction 作用域、优先级、长度、delimiter、冲突和 WorldState 刷新 snapshot。
- M4 增加“权限模式变化生成新 policy fragment，旧 session record 不被篡改”的验收。
- compaction 后只重新注入当时存在的 canonical Base/Tool/Policy/Project/WorldState，并保持顺序正确。

真实任务指标：

- Tool 选择错误率和不存在 Tool 调用率；
- 不必要 `AskUserQuestion` 次数；
- 未验证成功声明率；
- 无关文件修改率；
- prompt token 占比；
- 有/无 Tool guidance、ProjectInstructions、model overlay 的消融对比。

只有 eval 证明新增 prompt 规则改善指标，才进入 BaseAgentContract；临时模型 workaround 优先放 model overlay，并带删除条件。

## 5. P0：Context 与长任务能力

### A1.1 建立 ContextManager

当前 `max_prompt_bytes` 只按字节删除旧 turn，可能在长任务中丢失目标、约束和关键发现。

任务：

- 定义 `ContextBudget`：模型 context window、reserved output、tool schema、system prompt、history 分别计费。
- Provider 暴露 token estimator；缺失时使用保守 fallback，不再只依赖 bytes。DeepSeek 可加载官方 `tokenizer.json`（Hugging Face Rust `tokenizers`），但完整请求 token 还涉及 chat template、Tool schema 和 Provider framing，离线估算只能作为投影。流程为：projected provider request -> provider-specific estimator -> conservative safety margin -> 实际 `prompt_tokens` 回填 -> 按 model/profile/version 校准误差分布。必须启用 `stream_options.include_usage=true` 才能稳定获得 streaming usage 用于回填（当前 `DeepSeekChatRequest` 未设置该字段，是待修缺口）；不能用 `cl100k` 冒充 DeepSeek 精确计数，引入 tokenizer 后仍须用包含 Tool schema 的真实 API usage 验证误差。
- 将 context 组装拆为：system instructions、workspace state、durable memory、recent turns、pending tool turn。
- 每次请求产生 `ContextPrepared` 诊断数据：预算、保留/移除 turn、估算 token，不必将完整敏感内容写入事件。

### A1.2 实现事务化 Compaction

任务：

- pre-turn 达到阈值时压缩；Provider 报 context overflow 时允许一次 mid-turn compaction 后重试。
- 正常情况保留 system、当前用户目标、最近完整 turn、未决 ToolUse/ToolResult，只总结可压缩的完整历史区间。
- 增加 split-turn 路径：当单个 turn 已超过保留预算时，只能在 user/assistant/custom 等合法边界切分，绝不从 ToolResult 开始，也绝不留下没有对应 ToolResult 的 ToolUse；分别总结更早 history 与当前 turn prefix。
- 新增 `compactions.jsonl` 或等价 typed record，记录输入消息边界、摘要消息 ID、模型和版本。
- compaction 结果先验证再提交；失败不得破坏原 history。
- continuation/fork 加载祖先时能够复用已提交摘要，不能每次重新总结。
- 摘要 prompt 要求保存：目标、已完成工作、文件变更、关键命令结果、约束、未解决问题；禁止编造成功状态。
- 保存并累计 read/modified file 集合，避免多次 compaction 后丢失关键文件足迹。
- compaction 后按模式重新注入 canonical instructions 与 World State；pre-turn 和 mid-turn 的插入位置必须有 snapshot test。
- 尽量保留稳定 prompt prefix；上下文项顺序或内容没有变化时不得每轮重写，以保护 Provider prompt cache。
- “禁止编造成功”不全部交给摘要模型：工具执行状态、验证结果和文件变更从 durable event 生成结构化 facts，compactor 只压缩叙述，不能改写这些事实。

验收分两类，不得混在同一断言集中：

机制不变量（可断言保证）：

- 小 context fixture 能连续运行超过原 projection 极限。
- ToolTurn 永不被拆开，不产生孤立 ToolResult。
- 单个超长 turn 能安全压缩或明确失败，不会永久卡在 overflow/retry。
- compaction 失败、超时和无效输出不会污染 messages。
- compaction 记录可恢复；最多一次 overflow recovery。
- canonical instructions 重新注入位置正确，pre-turn 与 mid-turn 有 snapshot test。

语义保真（只能通过真实任务 eval 衡量，且为概率指标）：

- 关键约束保留率。
- 关键文件/决策召回率。
- unsupported success claim rate（编造成功状态的比例）。
- compaction 前后任务成功率差值。
- 多次运行的均值、方差和置信区间。

### A1.3 分层指令与 World State

A1.3 是 `ProjectInstructions` 与 `TurnWorldState` 的内容生产者；A0.5 只负责接收 typed inputs 并确定性渲染，不在两处重复维护发现和内容规则。

任务：

- 实现 user → workspace root → 当前工作目录的指令发现与优先级，支持 `AGENTS.md` 类文件；更深目录只覆盖其目录树内更浅层的规则。
- 普通 workspace `AGENTS.md` 只能作为有来源、受限额的低优先级工程上下文，不能替换 `BaseAgentContract/RuntimePolicyContext` 或授予能力。M4 的 `ProjectTrust` 额外门控 workspace-local prompt override、Hook、Skill、MCP 等可执行/扩展资源。
- 每段 project instruction 使用明确 delimiter 和绝对或工作区相对 source path；正文中的 “system/developer” 字样不改变真实优先级。用户当前请求更高，冲突时记录诊断并遵循更高优先级来源。
- 拒绝 workspace 外符号链接和超出作用域的 instruction source。
- 注入只影响本轮决策的 typed world state：workspace/cwd、OS、shell、git branch/dirty summary、context/compaction 状态；sandbox capability 和 durable `TaskState` 只在各自 producer 上线后加入。
- Tool catalog 不在 WorldState 重复；时间、完整环境变量、无界 git diff、完整 task history 和 secret 永不注入。
- 只在 source/digest 或 typed state 变化时产生新 snapshot，不能重写已提交的旧 system message。
- `ProjectInstructions/WorldState` 的 token/byte 配额、优先级和确定性裁剪由 A1.1 `ContextBudget` 提供；本节定义内容保留优先级和合法截断边界。具体算法在 A1.1 实现时用 fixture 决定，不在 A0.5 临时选择 LRU、recency 或任意字符串截断。

验收：

- 嵌套目录任务拿到正确作用域指令。
- 指令冲突和 workspace 外符号链接有回归测试。
- 相同发现结果和 world-state 输入生成相同 source/digest；没有变化时不重复刷新。
- 超预算裁剪保留来源边界、不截断为伪造指令，并能解释每段保留/移除原因。
- prompt snapshot 能解释每段上下文的来源；M4 后额外覆盖 ProjectTrust 与 capability 变化。

## 6. P1：Tool 执行系统

### A2.0 Tool 平台总体取舍：Codex 骨架 + Pi Tool UX

当前 `flash-tools` 只注册 `Read/Edit/Write/Glob/Grep/ListFiles/Bash`，而且都集中在一个约 1700 行的 `lib.rs` 中。主要问题不是 Tool 数量少，而是：

- schema 手写，容易与 Rust input struct 漂移；
- 所有 Tool 共用 shell 形态的 `ToolOutput`，文件读取、搜索和编辑也被迫表达为 stdout/stderr/exit code；
- 同步 `Tool::call` 一律由 `spawn_blocking` 包装，无法自然表达流式输出、长进程、stdin 和远端 Tool；
- `ToolRisk` 只能给调用贴一个标签，无法表达真实文件、进程、网络和外部状态 effect；
- Glob/Grep/Edit/Write 各自实现路径、匹配、写入和限制逻辑，没有统一的 symlink guard、输出 quota、原子写和 mutation queue；
- spec、handler、底层执行、权限判断混在同一层，未来 MCP 或扩展很容易形成安全旁路。

两个参考项目各自最值得学习的部分：

| 维度 | 学习 Codex | 学习 Pi | Flash Code 决策 |
|---|---|---|---|
| Tool 平台骨架 | spec/runtime 绑定、Router/Registry、Direct/Deferred/Hidden、统一 orchestrator、审批与 sandbox 重试 | 不复制复杂 runtime | 以 Codex 为主 |
| 内置 Tool 数量 | 核心能力与 hosted/MCP/协作 Tool 分源注册，按环境和 feature 暴露 | 默认只保留 `read/bash/edit/write`，只读套件再加入 `grep/find/ls` | 默认 catalog 保持小而稳定 |
| Tool 参数与结果 | 强 correlation、typed event、exec session、apply_patch | TypeBox schema、`offset/limit`、明确截断续读提示、text/image content、details、取消和 progress callback | 使用 Rust 强类型复刻这些语义 |
| 文件搜索 | Codex 使用 `ignore` 等成熟 crate | `find` 调 `fd`、`grep` 调 `rg --json`，尊重 `.gitignore`，有结果/字节/长行限制 | 首版优先 Rust 库；语义和性能不达标时使用受管 `rg/fd` backend |
| 精确编辑 | Codex 主推独立 `apply_patch`，先 parse/verify/effect/approve 后执行 | `edit` 支持精确替换并保留 BOM/换行、返回 diff | 第一阶段 `Edit` 只做单文件单次精确替换；patch 延后 |
| 长进程 | `exec_command` + `write_stdin`，handler 与 exec runtime 分离 | `bash` 单调用流式返回，截断全文落临时文件 | 第一阶段 `Bash` 只做 one-shot command；persistent session 延后 |
| 扩展性 | built-in/MCP/extension/hosted 统一注册和曝光 | Tool definition 可包装、替换 operations，extension 可 override | core 使用统一 erased registry；backend/extension 通过 trait 注入 |

总体原则：

1. **Tool 是模型可调用的产品能力，不是所有内部服务。** Policy、Sandbox、Checkpoint、Compaction、Context Builder、Audit、Artifact Store 都是 Runtime 服务，不暴露成普通 Tool。
2. **spec、执行器、底层 backend、编排器四层分离。** Tool 自身不得弹审批、选择 sandbox 或写 session。
3. **默认 Tool catalog 尽量小。** 能由 `Bash` 清晰完成的低频能力，不立刻做专用 Tool；当专用 Tool 能显著提高安全性、结构化结果或跨平台一致性时才新增。
4. **模型名稳定、内部实现可替换。** 模型可见名称固定为 `Bash/Read/Write/Edit/Glob/Grep/Agent/AskUserQuestion/TaskCreate/TaskUpdate/TaskList`；Rust module、event 和 backend 仍使用 `snake_case`。公开 Tool 名不随内部 handler 拆分而变化。
5. **内置、MCP、Extension、Hosted 只改变来源和信任，不改变执行主链。**

建议的内部边界：

```text
Provider ToolCall
  → ToolRouter（名称/namespace/当前 step exposure）
  → ToolRegistry（spec 与 erased executor）
  → TypedToolAdapter（反序列化、schema、canonical input）
  → ToolOrchestrator（hook/effect/policy/approval/grant/sandbox/audit）
  → ToolHandler（产品语义）
  → Backend（filesystem/process/MCP/hosted）
  → ToolResult + ToolEvent
```

`ToolRegistration` 至少包含：

```rust
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub output_schema: Option<serde_json::Value>,
    pub prompt_snippet: Option<String>,
}

pub struct ToolRegistration {
    pub identity: ToolIdentity,
    pub source: ToolSource,             // Builtin / Mcp / Extension / Hosted
    pub exposure: ToolExposure,         // Direct / Deferred / Hidden
    pub execution_mode: ToolExecutionMode,
    pub spec: ToolSpec,
    pub executor: Arc<dyn ErasedToolExecutor>,
}
```

`prompt_snippet` 是有严格字节上限的模型可见选择提示，不是第二份 schema、权限说明或可执行模板。只有当前 step 实际暴露的 Tool 才能把它贡献给 A0.5 `ActiveToolGuidance`；缺失时使用 `description` 的有界摘要，不要求所有 Tool 编写 snippet。

动态分发只保留在异构 Registry 边界；每个内置 Tool 内部仍使用强类型 input/output 和静态分发。用 `TypedToolAdapter<T>` 擦除具体类型，避免业务代码到处操作 `serde_json::Value`，也避免为追求泛型而让整个 Runtime 携带复杂类型参数。

### A2.1 异步、可流式 Tool API

目标接口应表达 Tool 生命周期，而不是用 `spawn_blocking` 包装全部能力：

```rust
#[async_trait]
pub trait TypedTool: Send + Sync + 'static {
    type Input: DeserializeOwned + JsonSchema + Send;
    type Output: Into<ToolResult> + Send;

    fn spec(&self) -> ToolSpec;
    fn execution_mode(&self) -> ToolExecutionMode;
    fn canonicalize(&self, input: Self::Input) -> Result<Self::Input, ToolError>;
    fn effects(&self, input: &Self::Input, context: &ToolContext)
        -> Result<EffectSet, ToolError>;
    async fn execute(
        &self,
        input: Self::Input,
        context: &ToolContext,
        events: &ToolEventSink,
    ) -> Result<Self::Output, ToolError>;
}
```

任务：

- 实现 object-safe `ErasedToolExecutor` 和 `TypedToolAdapter<T>`；Registry 不直接持有手写 `Value -> Value` handler。
- 内置同步文件 Tool 只把确实阻塞的短操作放进统一 blocking backend；Exec/MCP 使用原生 async，禁止每个 Tool 自己随意 `spawn_blocking`。
- 用 `ToolResult` 替换 shell-shaped 通用输出；内容至少支持 `Text/Json/Image/ArtifactRef`，details 使用内部强类型 enum 或版本化 JSON。
- Exec 的 exit code/stdout/stderr、Edit 的 diff/patch、Search 的 match/truncation 分别放在对应 details 中，不能把所有 Tool 伪装成命令执行。
- Bash stdout/stderr 在运行中发出 bounded delta，而不是结束后一次性发送。
- schema 使用 `schemars` derive 生成，并通过 `jsonschema` 在注册时编译、调用时验证，减少 serde input struct 与手写 schema 漂移。
- Registry 启动时拒绝重复规范名、非法 namespace、无效 schema、超过大小预算的直接暴露 Tool，以及超过长度/格式限制的 `prompt_snippet`。
- call id、timeout、cancellation、artifact quota 成为统一 ToolContext 字段。
- Tool 错误分类为 `InvalidInput/NotFound/PermissionDenied/PolicyDenied/Cancelled/TimedOut/OutputLimit/Backend/Protocol`；每个 terminal ToolCall 恰好生成一个 ToolResultMessage，错误也不例外。

### A2.2 Tool Hook 链

定义最小稳定 Hook：

- `before_model_request`
- `after_model_response`
- `before_tool_call`
- `after_tool_call`
- `on_session_start`
- `on_session_finish`

任务：

- 将只读 `observe` 与会改变执行语义的 typed handler 分开；observer 返回值永远忽略。
- 将 `before_tool_call` 再拆成 input transform 和 permission guard。transform 可以 rewrite input；permission guard 默认只能 `Abstain/RequireApproval/Deny`，不能扩大核心策略授予的权限。
- input rewrite 后必须重新执行 schema 校验、参数 canonicalization 和 effect/risk 计算；审批完成后禁止再修改 input，避免“批准 A、执行 B”。
- `after_tool_call` 可添加审计元数据或过滤模型可见结果，但不可伪造实际执行状态。
- 为每种可变事件定义合并语义：transform chain、first-block-wins、patch accumulation 或 cancel-first，不能由调用点临时决定。
- Hook 注册携带 source id、版本和作用域，错误与审计可以定位来源。
- Hook 有稳定顺序、timeout、cleanup、错误隔离和 fail-open/fail-closed 配置；permission handler 默认 fail-closed，纯 observer 失败只记录诊断。
- 先支持进程内静态注册；外部扩展复用同一协议，不开放任意 Rust 动态加载。

验收：

- Hook 能阻断调用、脱敏输出、记录审计。
- Hook panic/timeout 不会卡死 session。
- 任何 Hook rewrite 都不能绕过 PolicyEngine。
- 普通 Hook 无法把核心 `Deny` 或 `Ask` 降级成 `Allow`。

### A2.3 两层式安全并行

第一层先采用 Codex/Pi 已验证的简单模型：

- `ToolExecutionMode::ParallelSafe`：允许和其他 ParallelSafe Tool 共享执行门。
- `ToolExecutionMode::Sequential`：独占执行门；Bash、Write、Edit、未知/MCP Tool 默认属于此类。
- 同一批 Tool 可以按完成时间发 lifecycle/update 事件，但 ToolResult 必须按 Provider 原 call 顺序提交。
- 只要批次中出现需要全局顺序的 Tool，调度器可以选择整批串行，首版优先保证语义简单。

第二层为 EffectSet。注意 EffectSet 首要服务于权限系统（见 A3），并行冲突分析只是其衍生用途。默认只实现 `ParallelSafe/Sequential` 第一层；第二层路径/host 冲突分析永久保持实验性，仅在 eval 证明第一层成为并行瓶颈时才立项。不能仅凭 `ToolRisk::Read` 判断路径冲突：

```text
EffectSet:
  WorkspaceRead(paths)
  WorkspaceWrite(paths)
  Process
  Network(hosts)
  ExternalState(service)
  Destructive
  Unknown
```

调度规则：

- 多个确定只读且路径不冲突的 Tool 可并发。
- Write/Edit/Bash/Network/Unknown 默认串行。
- 有写冲突或外部副作用的调用保持 provider 原顺序。
- 并发执行可以按完成时间发运行事件，但 ToolResult 写入 history 必须保持 provider call 顺序。
- 一个调用失败不自动取消其他已批准只读调用；session cancel 必须取消全部 sibling。

验收：

- Read/Grep/Glob fixture 有可测的并行加速。
- 两个写同一文件永不并发。
- 事件顺序和最终 history 在多次运行中确定。
- 未声明或来自远端且没有可信 execution annotation 的 Tool 默认串行。

### A2.4 简化版 Bash Runtime

任务：

- 第一阶段只支持一次性 `Bash { command, timeout? }`；调用启动命令、流式返回有界输出，并等待退出或取消。
- 将 shell 构造、进程组、stdout/stderr、timeout、sandbox、环境变量和工作目录收敛为简单 `ExecRequest/ExecResult`。
- 暂不支持 persistent session、stdin、poll、terminate、TTY 和后台进程 handle；需要交互式进程的调用明确返回 Unsupported。
- 暂不同时暴露 argv/shell 两套复杂 schema；模型输入保持 command string，内部 shell 解析与权限 effect 计算仍必须规范化。
- 环境变量使用 allowlist，默认过滤常见 secret；审计日志必须脱敏。
- 定义内部 `SandboxBackend`，输入 `SandboxPlan`，输出经过能力探测的 spawn plan；平台库和命令行参数不能泄漏到 Agent/Policy 类型。
- macOS 保留并隔离当前 `/usr/bin/sandbox-exec`/Seatbelt backend，补 filesystem/network profile 与真实 socket/file 测试。
- Linux 首选组合 `bubblewrap` 的 namespace/mount 隔离、`seccompiler` 的 seccomp-BPF 和 `no_new_privs`；`landlock` 仅在内核 ABI/capability 满足时作为附加限制或明确的 fallback，不把单一 crate 宣称为完整沙箱。
- Windows 后续通过 `windows` crate 组合 restricted token、Job Object、AppContainer/ACL；在 backend 和跨平台 CI 完成前明确报告 Unsupported。
- sandbox 启动失败、版本过低或请求能力无法兑现时 fail-closed，并报告 `UnsupportedCapability`；不允许从“sandbox 不可用”静默降级为 unsandboxed。只有用户明确选择 `FullAccess` 且策略允许时才走 unsandboxed plan，这是与 sandbox 失败互不相关的另一条显式路径。
- 平台分阶段：M3 只完成 macOS 当前 backend 的 filesystem/network/socket 测试；Linux sandbox 与 Windows sandbox 各自单列为独立里程碑，backend 未通过 bypass 语料和平台 CI 认证前确定性拒绝，不混入 M3。真正风险不是“任意命令执行”（Bash 本就在执行命令），而是突破已授予的 filesystem/network/process/credential 边界。
- 先复用成熟原语并保持 backend 可替换，不复制 Codex 整套 sandbox 源码，也不把 `libc` syscall 拼装散落到 Tool 实现。

验收：

- 每个平台 capability probe 与实际限制测试一致，不能只检查二进制是否存在。
- Linux 分别验证 workspace 外写入、受限读取、socket、子进程提权和 namespace 逃逸回归语料。
- macOS 验证 workspace 写边界和真实 socket 拒绝；Windows 未实现时测试确定性拒绝。
- sandbox backend 的单元测试可以使用 fake backend，不要求 Agent 状态机测试依赖宿主平台。

### A2.5 合并后的模型可见 Tool catalog

P0/P1 不再按模型可见名称拆成两套 catalog。最终稳定界面统一为：

| Tool | 模型可见职责 | 内部组合能力 | 默认执行模式 |
|---|---|---|---|
| `Bash` | Executes Bash commands and returns their output. | one-shot command、streaming output、timeout、cancel、sandbox、artifact output | `Sequential` |
| `Read` | Views the raw contents of files. | text range read、encoding/binary detection、line/byte quota、fingerprint | `ParallelSafe` |
| `Write` | Generates new files or completely overwrites existing files. | create/overwrite、parent creation、atomic writer、fingerprint conflict | `Sequential` |
| `Edit` | Modifies a specific segment within a target file. | single-file exact replacement、fingerprint、atomic write、diff | `Sequential` |
| `Glob` | Finds files matching specified patterns across directories. | directory listing、glob walk、gitignore、stable sort、pagination | `ParallelSafe` |
| `Grep` | Searches for explicit text strings or regular expressions inside project files. | literal/regex、case、glob filter、context、match/byte/line quota | `ParallelSafe` |
| `Agent` | Spawns and manages nested subagents to parallelize deep contextual tasks. | spawn、send/follow-up、wait、list、interrupt、budget、permission narrowing | `Sequential` |
| `AskUserQuestion` | Pauses automation to prompt the user for critical clarifications, input, or decisions. | cancellable RuntimeCommand、call correlation、headless capability check | `Sequential` |
| `TaskCreate` | Creates a persistent multi-step workflow task. | durable TaskState create、stable task id、dependency validation | `Sequential` |
| `TaskUpdate` | Updates task status, description, dependencies, owner or progress. | optimistic version、state transition、audit event | `Sequential` |
| `TaskList` | Lists current persistent workflow tasks and their state. | filtered projection、stable order、bounded result | `ParallelSafe` |

这套 catalog 比提前暴露大量 Runtime 机制更合适，原因是：

- 模型只需学习稳定的产品语义，不需要理解 Runtime 的内部拆分；
- Tool schema 数量和每轮 context 成本更低；
- `Bash` 和 `Edit` 第一阶段保持单一职责，避免为了未来需求提前设计 action union；
- persistent workflow 使用 `TaskCreate/TaskUpdate/TaskList` 比单个 `update_plan` 更适合恢复、fork、多 Agent 和跨 turn 管理；
- `AskUserQuestion` 与权限审批仍是两条协议，避免把业务提问当成授权渠道。

第一阶段使用简单 schema：

```text
Bash:
  command: string
  timeout?: number

Edit:
  path: string
  old_text: string
  new_text: string
```

`Bash` 不保留进程 session；`Edit` 只执行一个文件中的一次唯一精确替换。后续若评测证明交互式终端、多处替换或 patch 能显著提高成功率，再单独设计兼容升级，不能因为“未来可能需要”现在就引入复杂 union schema。

#### 上线顺序与可用性门控

catalog 名称合并，但实现仍分阶段：

1. 第一批实现简化版 `Bash/Read/Write/Edit/Glob/Grep`，覆盖当前核心编码闭环。
2. Runtime 有可取消的 control bus 后启用 `AskUserQuestion`。
3. durable `TaskState`、恢复和事件协议完成后启用 `TaskCreate/TaskUpdate/TaskList`。
4. thread graph、子 Agent budget、权限收窄、取消传播和结果聚合完成后才启用 `Agent`。

未满足上线条件的 Tool 必须不注册/不暴露，不能提供一个调用后返回 “not implemented” 的空壳。这样既保留统一目标 catalog，也不允许 `Agent` 因过早上线绕过 session、权限和审计边界。

#### Read 与多模态

`Read` 的核心定义保持“查看文件原始内容”。文本直接返回有界内容；图片或其他二进制文件返回类型、大小和 artifact/image content（仅当 Provider 支持），不再额外暴露 `ViewImage`。解码、resize 或转码属于 `ReadBackend` 的受限媒体分支，不把图片编辑混入 `Read`。

#### Edit 与未来 Patch

第一阶段 `Edit` 只支持单文件、单个唯一 `old_text → new_text` 替换。`Write` 只负责新建或明确全量覆盖。

多处替换、multi-file patch、move/delete 暂不进入当前实现。未来若引入，优先评估独立 `ApplyPatch` Tool，而不是直接把复杂 action union 塞进已经稳定的 `Edit` schema。

#### Task 与 Agent 的关系

- Task 是持久化工作流状态，不等同于一次 Agent 调用。
- `Agent(action=spawn)` 可以绑定一个既有 task id，也可以由父 Agent 先调用 `TaskCreate`。
- 子 Agent 只能通过 `TaskUpdate` 更新被授予的任务，不能任意修改整个 task graph。
- `TaskList` 返回状态投影，不拼接子 Agent 全历史。
- Task 状态不是对话消息；ToolResult、TaskState event 和 durable task record 三者分别存储并通过 id 关联。

#### 外部和延迟发现 Tool

- MCP、Extension、Hosted Tool 不强行塞入以上核心名称；它们适配成统一 `ToolRegistration`，大量 schema 使用 `Deferred`。
- `tool_search` 只有在外部 Tool catalog 超预算时作为 Runtime discovery 机制启用，不属于默认核心 catalog。
- Hosted web search、image generation、浏览器、邮件、日历等继续由 Provider/Connector/MCP 提供。

#### 明确不做成核心 Tool

- `git_status/git_diff/git_commit`：通过 `Bash` 完成；status/diff 可作为内部 World State 或 FileChange event。
- 独立 `DeleteFile`：暂不提供；确需删除时通过受权限和 sandbox 约束的 `Bash` 完成，未来随 patch 能力重新评估。
- 独立 `ViewImage`、`ExecCommand`、`WriteStdin`、`UpdatePlan`：分别由 `Read`、简化 `Bash` 和 Task Tool 覆盖；第一阶段不存在交互式 stdin。
- Sandbox、权限申请、审批缓存、Checkpoint、Compaction、Artifact Store：属于 Runtime 服务。
- `current_time/sleep`：默认不是编码 Agent 核心能力。
- 任意 HTTP client：不提供绕过 MCP/Connector 和 network policy 的万能网络 Tool。

### A2.6 各 Tool 的实现、成熟库与参考方案

#### 文件路径与共享基础设施

所有文件 Tool 先经过同一个 `PathGuard`：

- lexical normalize 后解析 workspace-relative path；
- 对已存在的每级 ancestor 做 symlink/canonical path 校验；
- 对将创建的路径校验最近存在 ancestor，避免“目标不存在所以无法 canonicalize”的绕过；
- 明确是否允许 workspace 外读取/写入，不能由 Tool 自己猜；
- 将解析后的 `PathUri/AbsolutePathBuf` 放入 canonical arguments，effect、approval key 和实际执行使用同一对象，避免 TOCTOU。

依赖优先与 Codex workspace 对齐：路径模式用 `globset`，目录遍历和 `.gitignore` 用 `ignore`，regex 用 `regex`，diff 展示/比较用 `similar` 或已引入的 `diffy`。这些库必须封装在 `FsBackend/SearchBackend/DiffBackend` 后，业务协议不直接暴露第三方类型。

#### `Read`

- 学 Pi 的 `offset/limit`、1-based 行号、行数/字节双限额和可执行的继续读取提示。
- 文本解码首版只保证 UTF-8/UTF-8 BOM；检测 NUL 或无效 UTF-8 时返回 typed binary/encoding error，不静默损坏内容。
- 大文件按流/seek 读取所需区间，不能先把整个文件读入内存再 slice。
- 图片/二进制先返回受限 metadata；只有 Provider 支持 image content 时才通过媒体分支返回 image/artifact，避免非视觉模型收到大 base64。
- 返回文件 fingerprint（size/mtime/hash 可按风险选择），供后续 edit 的 optimistic concurrency 使用。

#### `Glob`

- pattern 允许精确目录或 glob；精确目录等价于原 `ListFiles` 的一层列举，glob 才进行递归 walk，避免再暴露独立 `ListFiles`。
- 使用 `ignore::WalkBuilder + globset`，尊重 `.gitignore`，默认排除 `.git` 和构建缓存；不要继续维护手写 glob matcher。
- 结果必须有 `limit/cursor/truncated`，排序必须确定；不能依赖文件系统枚举顺序。
- Pi 使用自动下载的 `fd` 是成熟且高性能的方案，但 Flash Code 的基础本地能力不应在首次调用时静默联网下载二进制。只有性能 benchmark 或语义兼容证明 Rust backend 不足时，才增加显式安装、校验版本/校验和的 managed `fd` backend。

#### `Grep`

- 参数对齐 Pi 的成熟 UX：`pattern/path/glob/ignore_case/literal/context/limit`。
- 首选两种 backend 做 spike 后用数据决策：
  1. Rust-native：`ignore` walker + `regex`，或引入 ripgrep 维护的 `grep-searcher/grep-regex` crates；
  2. managed process：调用版本受控的 `rg --json --line-number --color=never`，解析 JSON event。
- 如果采用 `rg`，它仍然是 `SearchBackend`，不经过模型可见 `Bash`；需验证路径、固定 argv、取消子进程、限制 stderr/JSON event/长行，且不能运行用户 shell。
- Pi 的可借鉴点是 match limit、全局字节限额、长行截断和 context 格式；不能照抄其按匹配再次完整读取文件的高内存路径。
- 增加 binary skip、encoding error、invalid regex、single huge line、symlink、hidden/gitignore 和 cancellation fixture。

#### `Edit`

- 第一阶段只接受 `path/old_text/new_text`；`old_text` 必须在原文件中唯一匹配。
- 保留 BOM 和原换行风格；预先计算 diff，全部验证通过后再写。
- 同一路径使用 mutation queue/lock，并比较 read fingerprint；文件在审批或写入前变化则返回 conflict，不能覆盖新内容。
- 使用同目录临时文件 + flush + rename 的原子写路径；权限/ownership 可保留则保留。不要继续直接覆盖目标文件。
- `prepareArguments` 一类模型兼容 shim 只能做版本化、可审计的字段迁移，迁移后仍严格 schema 校验，不能容忍任意畸形参数。

#### `Write`

- 明确 `create_only/overwrite` 或 `expected_fingerprint`，禁止调用者无意覆盖已存在文件。
- 自动创建父目录，但父目录和目标都必须先经过 PathGuard 与 effect 计算。
- 复用原子写 backend 和 mutation queue；取消发生在 rename 后时，结果必须报告“已提交”，不能返回一个让 Agent 误以为未写入的 Cancelled。
- 大内容受 Tool input/context quota 限制；生成超大或二进制文件应使用 artifact/import 路径，而不是 JSON 字符串。

#### 后续候选：`ApplyPatch`

- 本节不属于第一阶段 Tool 里程碑。只有真实任务评测证明单文件 `Edit` + `Write` 明显不足时才立项。
- 若立项，参考 Codex 将 patch 做成独立 freeform Tool，并严格分成：streaming preview parser → terminal parse → filesystem verify → effect/path permissions → approval → apply。
- 流式 arguments 只可用于 UI/FileChange preview，**绝不能在 ToolCall terminal、完整解析和授权前落盘**。
- 格式首版支持 add/update/move/delete；delete/move 和 workspace 外目标形成单独高风险 effect。
- Codex 的 `codex-apply-patch` 是其 workspace 内部 crate，并非应直接复制的通用依赖。Flash Code 应先评估：
  - 能否以许可证和维护成本可接受的方式抽取/复用该 crate；
  - 否则用内部严格 parser/grammar，`similar`/`diffy` 只负责 diff 算法或展示，不能替代安全验证。
- 多文件 patch 先完整预检所有文件和 preimage，再写临时文件；提交阶段使用 journal，失败可恢复或明确报告部分提交。不能声称普通文件系统 rename 能提供真正的跨文件原子事务。
- parser、path、overlap、move/delete、CRLF、EOF、partial commit 必须有 property/fuzz 与故障注入。

#### `Bash`

- 第一阶段每次调用只执行一个 command，并等待退出、timeout 或 cancel；不返回持久 session handle。
- 采用 Pi 的输出策略：运行中节流发送 update，最终只返回有界 tail；完整超限输出写 Artifact Store 并返回 ref，不直接暴露任意临时路径。
- schema 只包含 `command` 和可选 `timeout`；cwd 来自 ToolContext，不让模型任意切换未授权 workspace。
- `tokio::process` 负责异步进程 I/O，平台进程组/Job Object 由 `ProcessBackend` 封装；sandbox plan 必须由 Orchestrator 在 spawn 前确定。
- persistent session、stdin、poll、terminate、TTY 和后台 handle 只有在集成测试/真实任务证明必要后再设计。

#### `Read` 的媒体分支

- 只读取已授权本地文件，使用成熟 MIME 检测和 image decoder 校验；限制压缩前/后尺寸、像素数和字节数，防解压炸弹。
- 返回 `ToolResultContent::Image`，Provider adapter 再映射各厂商格式；Agent core 不保存无界 base64 到 JSONL，可保存 artifact ref。
- 是否 resize/转码由独立 `ImageBackend` 决定；若引入 `image` crate，应禁用不需要的 codec feature 并做安全/内存评估。

#### `AskUserQuestion`

- handler 只发 RuntimeCommand，不访问文件或 shell。
- 调用进入可取消等待状态，回复通过 control bus 关联 call id；headless/不支持模式应不注册，而不是调用后永久等待。
- 权限询问继续使用独立 `ApprovalRequested/ApprovalResolved` 事件和 `ExecutionGrant`，不允许 `AskUserQuestion` 返回值直接扩大权限。

#### `TaskCreate` / `TaskUpdate` / `TaskList`

- 使用独立、版本化的 `TaskStateStore`，不要把 task 列表编码进 system prompt、assistant message 或普通 session metadata。
- `TaskCreate` 验证标题、描述、依赖关系和最大 task 数，返回稳定 task id。
- `TaskUpdate` 使用 expected version 或等价 optimistic concurrency，校验合法状态转换、依赖完成条件和 owner 范围。
- `TaskList` 返回有界、稳定排序的结构化投影；历史详情通过内部 store 查询，不把无界审计记录塞给模型。
- 三个 Tool 都发 TaskState event；ToolResult 只确认本次调用结果，durable record 才是任务状态真相源。

#### `Agent`

- handler 只是 MultiAgentCoordinator 的 typed facade，不直接创建裸线程或拼接 prompt。
- `spawn` 必须指定目标、budget、允许的 workspace/effect 和可选 task id；子 Agent 临时批准不得从父级继承。
- `send/follow_up/wait/list/interrupt` 使用 opaque agent handle，并验证 parent session 的所有权。
- sibling 可并行运行，但 `Agent` ToolCall 自身默认 `Sequential`，避免同一父 turn 并发修改 agent/task graph。
- 子 Agent 返回结构化 summary、status、artifact refs 和 task updates；不把完整内部 transcript 自动注入父上下文。

#### MCP / Extension Tool

- MCP SDK 只负责 transport/protocol；转换成 `ToolRegistration` 后由统一 Registry/Orchestrator 调度。
- 远端 schema 在注册时限大小、深度和关键字；参数本地验证后再发送。
- annotation 只作为不可信 hint；默认 `Sequential + Unknown effect + Ask/Deny by policy`。
- Extension 可以包装 backend 或贡献新 Tool，但不能替换 Registry、直接写 transcript、跳过 hook/policy/sandbox。

### A2.7 Tool catalog、暴露与兼容迁移

任务：

- `ToolRouter` 每个 step 根据 Provider capability、运行模式、环境能力和 feature 生成模型可见 spec；Registry 可以保留 Hidden dispatch alias。
- 实现 `Direct/Deferred/Hidden`；首版不需要 `DirectModelOnly`，等 code mode/嵌套执行真的存在再增加。
- Tool source 使用 namespace，避免 MCP/Extension 覆盖 built-in；冲突默认拒绝，显式 override 仅用于受信测试/配置且记录审计。
- 记录每轮 tool catalog digest；恢复或 continuation 时能解释 schema 变化。
- schema description 也纳入 token/byte budget；Deferred Tool 通过 `tool_search` 进入后续 step，不修改已经发出的 Provider request。
- 迁移现有 `Read/Edit/Write/Glob/Grep/ListFiles/Bash`：保留六个同名 Tool，`ListFiles` 先注册 Hidden alias 并映射为 `Glob` 的精确目录模式；新增字段通过版本化兼容转换迁移。
- 不把模型产出的部分 ToolArgumentsDelta 交给 executor；只有 terminal arguments 经 parse、兼容迁移、schema validate、canonicalize 后才能计算 effect 和执行。

验收：

- 相同配置和环境生成稳定 catalog/snapshot。
- 模型无法调用未注册、Hidden 或尚未 discover 的 Tool。
- alias 与规范名执行得到相同 canonical input/effect/result，但模型只看到规范名。
- MCP/Extension 重名、超大 schema、畸形 schema 和运行中 catalog 变化有确定行为。

## 7. P1：权限、信任与审计

### A3.0 权限模型分层

Flash Code 当前把 `ApprovalMode`、Tool 风险和实际执行能力合并在一个判断中：

```text
ApprovalMode × ToolRisk → Allow / Ask / Deny
```

这会造成语义混乱：

- `Human` 实际是全部拒绝，名字无法表达真实行为。
- `Yolo` 仍然会询问 Network/Destructive，既不是无交互，也不能作为可靠 headless 策略。
- 自动 Allow 也写入 `ApprovalResolved { approved: true }`，但实际上没有发生用户审批。
- 单值 `ToolRisk` 无法表达一个调用同时具有 WorkspaceWrite、Process 和 Network effect。
- Allow/Ask/Deny 三条 Agent 分支重复 Tool 执行逻辑，后续 Hook、审计和 sandbox 容易产生差异。

学习 Codex 时，必须把“是否允许弹审批”和“执行能力上限”分开：

```rust
pub enum ApprovalPolicy {
    OnRequest,
    Never,
}

pub enum PermissionProfile {
    ReadOnly,
    WorkspaceWrite,
    FullAccess,
}
```

语义：

- `ApprovalPolicy::OnRequest`：PolicyEngine 返回 Ask 时可以向用户请求批准。
- `ApprovalPolicy::Never`：永不弹窗；Ask 自动转为 Deny，而不是自动放行。
- `PermissionProfile`：规定沙箱和执行环境能够授予的权限上限，即使用户批准也不能静默越过。
- `ProjectTrust`：只决定 workspace 本地配置、Hook、MCP server 等动态资源是否可以加载，不等于 Tool 调用已经安全。
- `PolicyEngine`：基于本次具体调用、规则和真实 sandbox capability 生成最终决策。

用户侧可以继续提供简化 preset，但 core 只接收明确的组合：

| 用户 preset | ApprovalPolicy | PermissionProfile | 行为 |
|---|---|---|---|
| `safe` | `OnRequest` | `WorkspaceWrite` | 已知只读自动允许，写入/执行根据规则询问 |
| `auto` | `Never` | `WorkspaceWrite` | 已被策略允许的 workspace 操作自动执行，需要升级的操作直接拒绝 |
| `read-only` | `Never` | `ReadOnly` | 只允许读取和分析 |
| `full-access` | `Never` | `FullAccess` | 明确危险；仍不能覆盖永久 Deny 和平台硬限制 |

`Confirm/Yolo/Human` 可作为短期 CLI 兼容别名，但不应继续出现在核心权限类型中。

### A3.1 PolicyEngine v2

将当前 `ApprovalMode × ToolRisk` 升级为：

```rust
pub struct PolicyInput {
    pub tool: ToolIdentity,
    pub source: ToolSource,
    pub arguments: CanonicalArguments,
    pub effects: EffectSet,
    pub project_trust: ProjectTrust,
    pub sandbox_capabilities: SandboxCapabilities,
}

pub enum PermissionDecision {
    Allow {
        sandbox: SandboxPlan,
        matched_rule: Option<RuleId>,
    },
    Ask {
        reason: String,
        suggested_rule: Option<RuleAmendment>,
        sandbox: SandboxPlan,
    },
    Deny {
        reason: String,
    },
}
```

任务：

- 明确 `Allow` 不等于绕过 sandbox；审批结论和 `SandboxPlan` 始终是两个字段。
- 多个 policy/interceptor 结果按 `Deny > Ask > Allow` 合并，普通扩展不能覆盖更严格结果。
- 规则作用域支持 once、session、workspace、user。
- `Allow Always` 只保存经过规范化的规则，不保存任意原始 shell 字符串。
- 命令规则基于解析后的 program/argv、工作目录和 effect；`bash -c`、解释器 `-e/-c`、命令替换等不能通过简单前缀获得永久放行。
- 网络规则独立表达 scheme/host/port，并与 Tool 权限分开。
- 路径/host 规则匹配采用 `globset`、URL 规范化采用 `url`；shell tokenization 优先复用 `shlex` 或经过验证的 parser，不自己写引号/转义解析器。
- 当规则需要理解 pipeline、重定向、subshell 或 command substitution 时评估 `tree-sitter-bash`；parser 无法完整理解的命令降级为 Ask/Deny，不能猜测 Allow。
- 每条决策记录 rule id、justification 和实际 sandbox capability。
- 策略文件原子写、权限收紧、拒绝 symlink 和 workspace 注入。

### A3.2 执行前权限拦截管线

结合 Codex 的不可绕过 Policy/Sandbox 骨架和 Pi 的顺序 Tool Hook，统一执行链定义为：

```text
1. 根据 tool name 解析 Tool 和来源
2. 首次校验 JSON Schema
3. 顺序执行 input transform hooks
4. 重新校验并 canonicalize 修改后的 input
5. Tool 计算 EffectSet
6. 检查 Project Trust 和 Tool 来源
7. PermissionProfile 检查能力上限
8. 加载 session/workspace/user policy rules
9. 顺序执行 permission guards
10. 按 Deny > Ask > Allow 合并结果
11. 用 ApprovalPolicy 处理 Ask
12. 生成不可变 ExecutionGrant
13. 按 SandboxPlan 执行已批准的精确请求
14. 执行 after_tool_call hooks
15. 提交 ToolResult 和审计事件
```

核心类型：

```rust
pub enum PermissionContribution {
    Abstain,
    RequireApproval { reason: String },
    Deny { reason: String },
}

pub struct ExecutionGrant {
    pub call_id: String,
    pub input_digest: InputDigest,
    pub decision: GrantedDecision,
    pub sandbox: SandboxPlan,
    pub effects: EffectSet,
}
```

约束：

- `ExecutionGrant` 绑定 call id、canonical input digest、effects 和 sandbox plan。
- grant 生成后任何 input 变化都会使 grant 失效并重新走完整权限链。
- permission guard timeout、panic 或协议错误默认 Deny；observer 错误不影响执行。
- 后置 Hook 可以脱敏模型可见输出，但不能把真实失败改写成审计层成功。
- MCP annotation、模型文本、仓库内容和 Tool 自报的“safe”只能作为输入信号，不能单独得到 Allow。

### A3.3 Approval Cache 与持久化规则

学习 Codex 的 session `ApprovalStore`，为不同 Tool 定义规范化 approval key：

- Read：operation + canonical path。
- `Write` 与 `Edit`：operation + canonical target path。
- Bash/Unified Exec：canonical executable + parsed argv/prefix + cwd + requested sandbox permissions。
- Network：protocol + normalized host + port。
- MCP：server identity + tool name + normalized effects；默认最多记忆到 session。

审批结果：

```rust
pub enum ApprovalDecision {
    AllowOnce,
    AllowForSession,
    AllowAlways,
    Deny,
}
```

规则：

- `AllowForSession` 只进入内存 ApprovalStore，不写磁盘。
- `AllowAlways` 只能应用 PolicyEngine 生成并通过 banned-prefix/规则验证的 amendment。
- `bash -c`、`node -e`、`python -c`、通用解释器、命令替换、未知 MCP 副作用不得生成宽泛永久规则。
- 多条规则同时命中时采用最严格结果。
- 持久化规则包含作用域、justification、来源和创建时间，并支持安全撤销。

### A3.4 权限与审批事件协议

自动策略决策与真实用户审批必须使用不同事件：

```rust
Event::PermissionEvaluated {
    call_id,
    decision,
    reason,
    rule_id,
}

Event::ApprovalRequired {
    approval_id,
    call_id,
    reason,
}

Event::ApprovalResolved {
    approval_id,
    decision,
    scope,
}
```

只有真正请求用户输入时才产生 `ApprovalRequired/ApprovalResolved`。自动 Allow、规则 Allow 和直接 Deny 只产生 `PermissionEvaluated`，从而保证 replay、审计和统计不会把自动决策误报为人工批准。

### A3.5 Project Trust

任务：

- 首次读取 workspace 本地配置、指令文件、MCP 配置或外部扩展前检查 trust。
- trust 记录 canonical path 和可选目录继承；workspace 移动或 inode/owner 异常时重新确认。
- 未信任 workspace 禁止自动启用本地 MCP server、Hook 和高风险命令。
- `doctor` 输出信任来源、当前策略和真实 sandbox 能力。
- 学习 Pi 的加载顺序：trust 决策前只允许全局/显式传入的可信资源参与，project-local Hook/扩展必须在 trust 通过后加载。
- 不照搬 Pi 的同进程任意 TypeScript 扩展安全边界；Project Trust 只保护加载时机，不能把扩展本身视为 sandbox。

### A3.6 审计与 Secret 防护

任务：

- 对 API key、authorization header、常见 token 环境变量做统一 redaction。
- CredentialStore 优先封装与 Codex 一致的 `keyring` backend；内存 secret 先采用 `zeroize` 减少残留，并通过不实现 Debug/Serialize 的自有 wrapper 控制泄漏。只有 spike 证明有额外收益时再引入 `secrecy`，且仍需控制 String、HTTP buffer 等额外副本。
- 审计 Provider、Tool、Hook、MCP 的开始/结束、耗时、状态和决策，不记录无界原文。
- 敏感 tool output artifact 使用明确权限；日志和错误不泄漏 credential。
- 增加 prompt injection 测试：仓库文件不能改变权限规则或冒充 approval。
- 审计记录必须能回答：哪个 policy/interceptor 做出决定、是否真实询问用户、采用了哪个 sandbox plan、最终执行的 input digest 是否与批准对象一致。

权限系统整体验收：

- `ApprovalPolicy::Never` 遇到 Ask 时确定性 Deny，不会弹窗、等待或自动放行。
- `PermissionProfile::ReadOnly` 即使收到用户批准也不能执行 Write/Process/Network。
- 普通 Hook 只能保持或收紧核心决策，不能将 Ask/Deny 降级为 Allow。
- transform hook 修改 input 后会重新校验和重新计算 effects；修改后的非法参数不会进入 PolicyEngine。
- approval 后发生 input、cwd、target path 或 sandbox request 变化时，原 `ExecutionGrant` 无效。
- 自动 Allow/Deny 不产生伪造的 `ApprovalResolved`；live event 与 replay 审计一致。
- session approval cache 只命中规范化相同的请求；不同 cwd、path、argv、host 或 MCP server 不会误复用。
- 永久 exec/network rules 通过 bypass corpus，禁止解释器、shell wrapper 和命令替换获得宽泛 Allow。
- Project Trust 失败时不会加载 project-local Hook、MCP server 或动态 Tool。

## 8. P1：Session、Thread 与运行恢复

### A4.1 从 Session ancestry 升级为 Thread Graph

任务：

- 引入稳定 `ThreadId` 与每次运行的 `SessionId`，避免把“对话身份”和“一次执行记录”混为一谈。
- 记录 `parent_thread_id`、`forked_from_message_id`、model、policy、instruction snapshot。
- 支持从任意完整 message/turn 边界 fork；禁止从半个 ToolTurn fork。
- 提供 list/get/fork/continue API，CLI 只是这些 API 的调用者。
- ancestry 深度、循环和跨 workspace 继续沿用现有防护。

### A4.2 Steering 与 Follow-up

语义必须区分：

- Steering：尽快影响当前运行；只在安全边界注入，不能打断正在提交的 Tool transaction。
- Follow-up：当前任务终结后开启下一 turn 或 child session。

任务：

- 两个有界队列，定义去重、顺序、容量和 cancel 行为。
- Steering command 必须携带 `expected_turn_id`；目标 turn 已结束或当前为 compact/recovery 等不可 steering 状态时明确拒绝，绝不自动投递到下一 turn。
- steering 在 provider/tool 边界读取；若要中断 provider，必须明确使当前 attempt 失效。
- 所有注入消息持久化来源和时间，replay 能解释行为变化。

### A4.3 Durable Checkpoint 与幂等恢复

当前崩溃恢复会将遗留 running session 标记 failed，安全但不能续跑。

任务：

- 记录 checkpoint：turn、request attempt、pending tool calls、已提交结果。
- 只有 ReadOnly/idempotent Tool 允许自动重放。
- Write/Execute/ExternalState 崩溃后必须进入 `NeedsReview`，不能猜测是否执行成功后再次执行。
- 使用 call id/idempotency key 防止外部 Tool 重复副作用。
- 从 checkpoint 恢复仍通过完整 Policy/Hook 链。

验收：

- Provider 完成前崩溃可从最后提交消息继续。
- Tool 执行中崩溃不会自动重复写文件或重复外部请求。
- 恢复结果有清晰、可审计的状态。

## 9. P2：MCP 与扩展机制

### A5.1 MCP Client

实现选型：

- 基于 `rmcp` 的稳定 release line 做一轮独立 spike，复用协议类型、capability negotiation 和 transport framing。
- 在 `McpClient`/`McpTransport` 适配层内隔离 `rmcp` 类型；SDK major 变化不能迫使 `flash-core`、Session/Event schema 一起变化。
- 不追随 prerelease 只为获得非必需功能；固定 feature 和 lockfile，并用 MCP contract fixture 验证升级。
- SDK 不负责 Flash Code 的 Project Trust、PolicyEngine、quota、catalog cache 和 ToolResult 提交，这些继续由 Runtime 掌控。

第一版范围：

- stdio transport。
- tools/list、tools/call。
- server initialize、capability negotiation、health、timeout、cancel。
- server 名称空间，例如 `server.tool`，禁止与 builtin 静默冲突。
- required/optional server：required 启动失败阻止 session，optional 只产生诊断。
- tool catalog revision 和 `list_changed` 刷新；刷新采用 generation，旧请求结果不能覆盖新 catalog。

第二版范围：

- Streamable HTTP transport。
- resources/list/read 与 prompts/list/get。
- OAuth/credential provider 和按 server 的网络规则。

强制约束：

- MCP Tool descriptor 转换为统一 ToolDescriptor 并验证 schema。
- Tool 暴露支持 `Direct/Deferred/Hidden`；大量或大 schema Tool 默认可 deferred，并通过 Tool Search/Discovery 按需加入当前 step。
- 单 Tool schema、单 server schema 总量和全 step Tool schema 都有硬上限；超大 description 可安全压缩，但不能改变参数语义。
- MCP 调用必须经过 Hook、PolicyEngine、quota、timeout 和审计。
- server 输出、progress 和 artifact 有大小上限。
- workspace 本地 MCP 配置受 Project Trust 控制。
- server 崩溃只失败相关调用，不使 Runtime 进程崩溃。
- catalog cache key 包含 transport、server config 和运行环境；会影响权限/并行的 live annotations 不进入跨连接缓存。
- MCP annotation 只能作为提示，不能单独把远端 Tool 升级为自动批准或 ParallelSafe。

### A5.2 外部扩展协议

任务：

- 以 manifest + 进程外 JSON-RPC/MCP 为扩展边界。
- manifest 声明名称、版本、命令、能力、配置 schema 和权限需求。
- 安装与启用分开；已安装扩展不能默认在所有 workspace 启用。
- 锁定来源、checksum/version，并提供禁用和故障隔离。
- Extension 只能通过公开 Hook/Tool/Event API 工作，不能直接写 session 日志。

不做：

- 不直接加载不稳定 Rust dylib。
- 不允许扩展绕过 PolicyEngine。
- 不在第一版同时实现 marketplace、自动更新和复杂依赖解析。

## 10. P2：Provider 层演进

### A6.1 诚实的 Capability Contract

当前应继续明确为 DeepSeek/OpenAI-compatible，而不是宣称已 provider-neutral。

任务：

- 定义 Runtime 实际使用的能力：streaming、tool calls、parallel tool call hint、reasoning content、max context、max output、usage。
- Provider 公共输出统一为 A0.4 的 `ModelStreamEvent`；DeepSeek/OpenAI DTO、SSE event name 和 finish reason 不得进入 Agent/Session 类型。
- 请求方向通过 `ProviderRequestProjector` 将 canonical `AgentMessage` 转为 provider wire message，学习 Pi 只在 LLM call boundary 执行 `convertToLlm`。
- 响应方向通过有状态 reducer 将 `ModelStreamEvent` 聚合为 completed item/message，学习 Codex 以 item id 管理 Added/Delta/Done。
- Provider 创建时协商能力；不支持的选项在请求前报错。
- opaque reasoning/signature 只有在能安全 round-trip 时才进入 namespaced `ProviderOpaqueState`，换 Provider 时默认移除。
- stop reason、tool id、usage 和 context overflow 做统一语义映射。
- provider adapter 不直接发布 RuntimeEvent；只有 Agent reducer 可以把已验证的 model event 投影为 Runtime item event。

### A6.2 第二 Provider 验证抽象

- 先实现一个 OpenAI-compatible adapter 或完整协议 fixture，不追求一次支持 30 个 Provider。
- 用同一组 provider contract tests 验证：item lifecycle、text、reasoning summary/raw、single/multiple tools、tool argument delta、refusal、length、usage、cancel、retry、malformed stream 和 early EOF。
- 只有第二个真实协议证明公共抽象不足时，才扩展 core 类型。

### A6.3 Credential 与模型配置

- CredentialResolver 支持 env、通过 `keyring` 访问的系统 keychain 或外部命令，secret 不写 session。
- CredentialResolver 依赖内部 `CredentialStore` trait，测试使用内存 fake，避免测试读写用户真实 keychain。
- model profile 描述 context、输出、reasoning/tool 能力和默认 timeout。
- API key 动态刷新，认证失败不透明重试。
- session snapshot 记录非敏感配置，保证 replay/诊断可解释。

## 11. P2：Agent 任务完成质量

### A7.1 计划、进度与停滞检测

- 增加 runtime-level task state：目标、当前步骤、验证状态、阻塞原因。
- 连续重复同一 Tool/input、无文件变化、相同错误循环时触发 no-progress policy。
- max turns 之外增加 token、wall-clock、tool calls、cost 等预算。
- 预算耗尽返回结构化 `BudgetExceeded`，不能伪装成功。

### A7.2 原生代码修改与验证能力

- 第一阶段使用简单 `Edit` 精确替换和 `Write` 全量写入，不把 patch engine 作为完成前置。
- 通过真实任务失败样本评估独立 `ApplyPatch`；只有收益明确后才建设 hunk 校验、workspace guard、可恢复提交和结构化 diff。
- Edit/Write 前后记录文件 fingerprint，检测并发修改。
- 建立 verification policy：代码发生变化后，根据项目类型建议或执行最小相关检查。
- Tool 结果向模型提供结构化 exit/diff/diagnostic，减少解析自由文本。

### A7.3 可选择的持久记忆

- 只存用户明确允许的 workspace facts/preferences，不自动把完整对话变成长时记忆。
- memory 带来源、作用域、更新时间和失效条件。
- ContextManager 决定检索和注入；被删除或不可信内容不能继续进入 prompt。
- memory 与 compaction 分开：compaction 是会话内部摘要，memory 是跨会话显式知识。

## 12. P3：Daemon 与多 Agent

### A8.1 Agent Daemon

只有在后台任务、多客户端或多 Agent 确有需求时实施。

任务：

- 单 workspace/session writer ownership，避免多个进程同时追加同一 JSONL。
- IPC 协议覆盖 start/attach/events/commands/list/fork。
- 客户端断开不默认取消任务；明确 detach/cancel 语义。
- daemon 崩溃走 durable checkpoint/recovery。
- 协议版本化并生成客户端类型或 schema。

### A8.2 多 Agent 调度

任务：

- 子 Agent 是带 `parent_thread_id` 的正常 thread/session，不建立第二套存储。
- 对模型只暴露统一 `Agent` Tool；spawn/send/follow_up/wait/list/interrupt action 适配到同一个 coordinator。
- 父 Agent 通过 `Agent(action=spawn)` 创建受限子任务，并可绑定 `TaskCreate` 返回的 task id。
- 限制深度、并发数、token/tool/time budget，取消向下传播。
- 子 Agent 默认使用更窄 workspace/effect 权限；不能继承父级临时批准。
- 父级只接收结构化结果和必要 artifact 引用，不直接拼接全部子会话历史。
- Event 中表达 agent id、parent id、状态和结果。

验收：

- 并行只读研究任务可获得确定结果。
- 一个子 Agent 失败不破坏其他 session 日志。
- 权限、预算、取消和审计均可沿父子链追踪。

## 13. P0-P3 工程质量与评测

### A9.1 契约测试

必须长期保留：

- Provider contract suite。
- ModelStreamEvent 状态语法、唯一 terminal、early EOF 和 terminal-after-delta 契约。
- provider wire → ModelStreamEvent → AgentMessage → RuntimeEvent 的跨层 golden fixture。
- role-specific AgentMessage 非法组合的 compile-time/API tests，以及 message/event serde version snapshots。
- partial ToolCall scratch state 不进入 durable message/session 的回归测试。
- System Prompt 契约按阶段测试：M1 覆盖 assembler/Base/当前 Tool guidance/最小 WorldState 与 source/digest；M2 增加 ProjectInstructions scope、裁剪和 compaction reinjection；M4 增加 policy/trust/capability filtering。
- Tool schema/input/effect 契约。
- Tool catalog exposure、namespace、alias 和 schema budget snapshot。
- 每个文件 Tool 的 path/symlink/workspace boundary、output limit、encoding、cancellation 契约。
- `Read` offset/limit/continuation、`Glob` gitignore、`Grep` regex/literal/context/long-line golden fixture。
- `Edit` unique-match/fingerprint/CRLF/BOM、`Write` create/overwrite/atomic commit 契约。
- `Bash` one-shot command、timeout、cancel、process-group cleanup、stream throttle 和 artifact truncation 契约。
- `AskUserQuestion` call correlation/cancel/headless，以及 Task Tool 的版本冲突、状态转换、依赖图和恢复契约。
- `Agent` 的 handle ownership、budget、权限收窄、取消传播、task scope 和结构化结果契约。
- Hook rewrite 后重新校验和权限复判。
- ApprovalPolicy/PermissionProfile 组合矩阵。
- permission interceptor 严格合并、timeout 和 fail-closed。
- approval key canonicalization、ExecutionGrant input digest 与 TOCTOU 防护。
- exec/network rule normalization、banned-prefix 和 bypass corpus。
- Project Trust 资源加载顺序。
- Tool parallel scheduling determinism。
- compaction/continuation/fork history invariant。
- Policy rule normalization 和 bypass corpus。
- MCP malformed/crash/timeout/oversize fixture。
- checkpoint crash point fault injection。

### A9.2 真实任务评测

将 eval 从“能运行”升级为可决策指标。扩展 `flash-eval` 现有 task/result/metric/report（已含 fixture、TerminalBench、SWE-bench、regression/trend、success/duration/command/token/failure category 和 event replay 路径），不建立第二套 harness；compaction fidelity 作为新的 benchmark suite 和 metric dimension 加入：

- task success rate。
- 首次修复成功率。
- 平均 turn/tool/token/time。
- compaction 前后成功率与信息保真。
- 并行 Tool latency 改善。
- permission ask 次数与误放行/误阻断。
- retry、cancel、recovery 成功率。

每个进入默认路径的功能必须绑定一种明确证据：硬性契约、不变量测试、安全语料或真实任务指标；纯优化必须证明可测收益。不是所有架构功能都要绑定业务性能指标，但都不能无证据进入默认路径。例如：四层消息绑定 terminal 唯一性、错误流拒绝、live/committed 一致性；Thread graph 绑定 fork 边界正确、恢复一致、无孤立 ToolTurn；Daemon 绑定 attach/recovery 成功率、单写者不变量、崩溃恢复；Tool 并行绑定延迟收益和结果确定性；Compaction 绑定机制不变量 + 语义保真 eval；Sandbox 绑定 bypass 语料和平台 CI。

### A9.3 可观测性

- 使用 `tracing`/`tracing-subscriber` 为 turn、provider attempt、tool、hook、policy、compaction、MCP 建 span。
- 日志使用结构化字段和稳定 error code。
- 默认本地、可关闭、无敏感正文；远程 telemetry 必须 opt-in，确认有导出需求后再增加 OpenTelemetry，避免首版引入完整 telemetry 依赖树。
- 提供 session inspect/export，能还原“为何调用、为何批准、为何失败”。

### A9.4 供应链与发布

- 引入 `cargo-deny`/`cargo-audit`，检查漏洞、license、重复和禁止来源。
- 锁定 `Cargo.lock` 并在 CI 使用 `--locked`。
- 第三方依赖在 workspace 统一版本和 feature，业务 crate 不各自漂移版本。
- 新依赖必须记录用途、维护状态、license/MSRV/native 风险、启用 feature、替代方案和退出策略。
- 安全关键依赖升级必须运行 sandbox/policy/MCP bypass corpus；不能只通过编译测试。
- 建立定期依赖升级任务；patch/minor 可以自动提 PR，但合并仍需完整门禁，major 和 sandbox/MCP/crypto 依赖必须人工评审。
- MCP/扩展进程视为不可信输入源，不能因为来自已安装包就提高权限。

## 14. 建议里程碑和依赖顺序

| 里程碑 | 包含任务 | 前置 | 完成信号 |
|---|---|---|---|
| M1 内核可演进 | A0.1-A0.5（prompt 仅 M1 范围） | 无 | 模块拆分、RunHandle/control bus、canonical message、轻量 stream contract，以及 prompt assembler/Base/现有 Tool guidance/最小 WorldState 的稳定 snapshot 完成 |
| M2 长任务可靠 | A1.1-A1.3 | M1 | token-aware context、事务化 compaction、ProjectInstructions 与当前可得的扩展 WorldState producer 接入，并有确定性预算/裁剪 |
| M3 Tool 平台 | A2.0-A2.7 | M1 | typed/erased Tool Registry、含 `prompt_snippet` 的正式 ToolSpec、稳定 catalog、async/streaming Tool、Hook、安全调度，以及简化版 Bash/Read/Write/Edit/Glob/Grep；macOS sandbox filesystem/network/socket 验证完成，Linux/Windows 保持 fail-closed `UnsupportedCapability` |
| M3-Linux Linux 沙箱 | Linux SandboxBackend、bubblewrap/seccomp/Landlock capability probe | M3，可与 M4/M3-Windows 并行 | Linux CI 与 bypass corpus 通过；要求沙箱的调用不再返回 UnsupportedCapability |
| M3-Windows Windows 沙箱 | Windows SandboxBackend、restricted token/Job Object/AppContainer | M3，可与 M4/M3-Linux 并行 | Windows CI 与 bypass corpus 通过；要求沙箱的调用不再返回 UnsupportedCapability |
| M4 安全闭环 | A3.0-A3.6 | M3 | 分层权限、不可绕过拦截链、持久化策略、Project Trust、统一审计，以及由 canonical policy/trust/sandbox capability 生成的 RuntimePolicyContext |
| M5 会话控制 | A4.1-A4.3 | M1、M2、M4 | message-boundary fork、steering/follow-up、幂等恢复 |
| M6 外部生态 | A5.1-A5.2 | M3、M4 | MCP stdio 工具经过统一权限链稳定运行 |
| M7 Provider/质量 | A6、A7、A9 | M2-M4，可并行推进 | 第二 Provider 契约通过，真实任务指标改善 |
| M8 后台与协作 | A8 | M5、M6、M7 | daemon 与多 Agent 不建立旁路状态/权限系统 |

平台沙箱依赖补充：M3-Linux 与 M3-Windows 都依赖 M3 提供稳定的 `SandboxBackend/SandboxPlan` 接口，两者互不依赖、可并行，且都不阻塞 macOS 上的 M4；但任一平台在启用完整 `WorkspaceWrite` 模式前，还必须通过 M4 的 Policy/ExecutionGrant 集成测试——共享接口有依赖，平台实现无顺序依赖。

关键路径：

```text
模块拆分与 RunHandle
  → Canonical Message + ModelStreamEvent + Runtime Item
  → Context/Compaction
  → Async Tool + Hook + Effect
  → Policy/Trust
  → Thread/Steering/Checkpoint
  → MCP
  → Daemon/Multi-Agent
```

Provider contract、eval、observability 从 M1 开始持续并行，不应等到最后补。

## 15. 近期可直接执行的任务清单

建议先开以下 31 个独立、可验收任务：

1. `refactor(agent): split runtime state machine into focused modules`
2. `refactor(storage): split session logs recovery and atomic metadata`
3. `refactor(tools): isolate fs tools exec process and sandbox backends`
4. `feat(runtime): introduce RunHandle and async command bus`
5. `feat(runtime): make approval command-driven and cancellable`
6. `feat(context): add model-aware context budget diagnostics`
7. `feat(context): add transactional pre-turn compaction`
8. `feat(context): recover once from provider context overflow`
9. `refactor(tools): add ToolRouter registry source exposure and hidden alias model`
10. `feat(tools): add typed Tool adapter and erased async executor boundary`
11. `feat(tools): derive compile and budget schemas with schemars and jsonschema`
12. `refactor(tools): replace shell-shaped output with typed ToolResult content and details`
13. `feat(tools): add shared PathGuard atomic writer mutation queue and output quota`
14. `feat(tools): rebuild Read Glob and Grep contracts on mature search libraries`
15. `feat(tools): make simple Edit unique-match conflict-safe and atomic`
16. `refactor(exec): make Bash one-shot async streaming cancellable and sandboxed`
17. `feat(tools): add ordered execution modes and parallel read-only tools`
18. `feat(agent): add ordered tool hook chain with revalidation`
19. `refactor(policy): split approval policy from permission profile`
20. `refactor(agent): unify allow ask and deny tool dispatch paths`
21. `feat(policy): add restrictive permission interceptor chain`
22. `feat(policy): add session approval cache and canonical approval keys`
23. `feat(policy): add scoped persistent exec and network rules`
24. `chore(deps): align workspace dependencies and features with codex baseline`
25. `feat(sandbox): add capability-probed linux backend with bubblewrap landlock and seccomp`
26. `spike(mcp): validate stable rmcp behind internal client and transport traits`
27. `refactor(protocol): introduce role-specific canonical agent messages`
28. `refactor(provider): replace flat provider events with item-based model stream events`
29. `feat(runtime): add item started delta completed lifecycle with stable correlation ids`
30. `feat(runtime): add AskUserQuestion and persistent TaskCreate TaskUpdate TaskList tools`
31. `refactor(prompt): add versioned prompt assembler base tool guidance minimal world-state and snapshots`

每个任务都必须：

- 先增加失败测试或 fixture。
- 不修改 TUI 视觉行为。
- 保持旧公开行为，除非任务文档明确声明破坏性变更。
- 通过：

```bash
cargo fmt --all -- --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features
```

## 16. 暂不优先的事项

- 复制 Codex 的 crate 数量或 daemon 形态，而没有后台/多客户端需求。
- 一次接入大量 Provider；先用第二 Provider 证明抽象。
- 在 Tool/Policy 边界稳定前做 marketplace。
- 以命令字符串黑名单代替真正的 effect、policy 和 sandbox。
- 把 compaction 与跨会话 memory 混为一个系统。
- 未定义确定性和副作用规则就并行所有 Tool。
- 为了插件能力直接加载 Rust 动态库。
- 在 durable checkpoint 之前实现自动恢复执行中的写操作。
- 自研 JSON Schema validator、glob/ignore walker、SQLite driver、MCP wire protocol 或 Secret Store。
- 为追逐最新版而直接采用 prerelease SDK，或让第三方库类型进入稳定 core/event 协议。
- 宣称存在一个“跨平台 sandbox crate”即可同时解决 filesystem、network、process、credential 和平台差异。

## 17. 最终完成定义

当以下条件同时满足，才能认为非 TUI Agent 内核改进完成：

1. 长任务能通过 token-aware compaction 继续运行，且不破坏 ToolTurn 或早期关键约束。
2. Tool catalog 小而稳定，spec/handler/backend/orchestrator 分层；Tool 异步、可取消、可流式、可 Hook，并只在可信 execution metadata/effect 证明安全时并行。
3. builtin、MCP、扩展 Tool 经过同一个 schema、Policy、Sandbox、quota 和审计链。
4. 审批规则可按作用域安全持久化，Project Trust 阻止 workspace 配置静默提权。
5. Thread 支持 message-boundary fork、steering、follow-up 和可审计恢复。
6. 至少两个 Provider/协议 fixture 通过同一 contract suite，公共能力没有虚假字段。
7. daemon 和多 Agent 即使实现，也复用同一个 Session/Event/Permission 模型。
8. 所有新增能力在真实任务评测上证明可靠性、成功率或延迟收益。
9. 全量质量门禁、故障注入、安全绕过语料和跨层集成测试持续通过。
10. 基础设施优先由通过准入评审的成熟库/OS 原语承担，安全关键依赖被内部 trait 隔离，并有可替换、可降级、可审计的 backend。
11. System Prompt 保持短小、来源可解释、能力感知且可版本化；安全由 Runtime 强制，新增规则通过 snapshot 或真实任务 eval 证明价值。
