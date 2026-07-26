# Agent 架构、Tool 设计与流式消息链路审查

> 状态说明（2026-07-26）：第 1-12 节记录重构前审查基线，第 13-17 节定义目标与方案，第 18 节记录本轮不兼容重构后的实现结果和最终验收方法。

## 1. 审查范围

本次审查覆盖当前 `feature/init` 分支中的以下模块：

- Agent Runtime 与模型循环
- Provider 抽象及 DeepSeek 实现
- Tool 协议、Registry、权限决策和执行链路
- Session、Message、Event 的持久化
- SSE 与流式事件传递
- TUI 实时渲染、取消和会话回放
- 0.9 重构技术计划与当前实现的一致性

审查时执行了完整测试和静态检查：

- `cargo test --all-features`：110 个测试全部通过
- `cargo clippy --all-targets --all-features -- -D warnings`：通过

测试通过说明当前已有行为相对稳定，但不能证明真实 Provider 与 Tool 调用链路正确。现有测试主要覆盖 mock、SmokeProvider 和旧字符串 Tool 输入，没有覆盖多个关键的跨层协议契约。

---

## 2. 总体结论

当前架构的分层方向合理，但实现仍属于“结构较完整的 Agent 原型”，还不能认为是真实 Provider 场景下稳定、可靠、安全的 Agent Runtime。

关键判断如下：

- Agent、Provider、Tool、Storage、CLI/TUI 的职责边界基本正确。
- Agent loop 集中在一个 Runtime 中，外围没有复制多套执行逻辑。
- 流式 Event 与最终 Message 分离的设计方向正确。
- 真实 DeepSeek Tool Calling 当前存在阻断级协议问题，实际上不可用。
- Tool 权限模型表面完整，但 Bash 执行层没有形成与之匹配的安全边界。
- 流式消息存在 UTF-8、重试重复、同步背压、取消失效和状态不一致问题。
- Provider 抽象目前主要适配 OpenAI-compatible Chat Completions，并非完整的 provider-neutral Agent 协议。

当前代码不需要推倒重来。宏观拆分可以继续沿用，重点应放在修复跨层契约和引入明确状态机。

---

## 3. 当前架构

### 3.1 静态模块关系

```text
flash-core
├── protocol
│   ├── Message / Role / ContentBlock
│   ├── Event
│   └── Outcome / SessionStatus / ToolResultStatus
├── storage
│   ├── .flash/workspace.json
│   └── sessions/<id>/
│       ├── session.json
│       ├── messages.jsonl
│       ├── events.jsonl
│       └── artifacts/
├── tools
│   ├── Tool trait
│   ├── ToolRegistry
│   └── PermissionPolicy
└── config / workspace discovery

flash-provider
├── ChatProvider
├── ChatRequest / ProviderEvent
├── ToolSpec / ToolCall
├── StopReason / Usage / ProviderError
└── Provider / ModelCapabilities

flash-deepseek
├── reqwest HTTP client
├── DeepSeek request/message/tool DTO
├── SSE parser
└── ProviderEvent 转换

flash-tools
├── Read/Edit/Write/Glob/Grep/ListFiles/Bash
├── legacy alias tools
└── workspace path guard / shell execution

flash-agent
└── AgentRuntime<P: ChatProvider>
    ├── session 创建
    ├── history 管理
    ├── provider turn loop
    ├── retry
    ├── event 持久化与 observer
    ├── approval
    ├── tool execution
    └── output artifact

flash-cli / flash-tui / flash-eval
└── Runtime adapters
```

总体依赖方向为：

```text
core ← provider ← deepseek
  ↑        ↑
tools ──→ agent ← cli / tui / eval
```

### 3.2 一次任务的运行链路

```text
CLI / TUI
  │
  ├── 加载 Config
  ├── 创建 builtin_registry()
  ├── 创建 Provider
  └── AgentRuntime::run_task_with_controls()
       │
       ├── create_session_async()
       ├── append_user_message_async()
       ├── for turn in 1..=max_turns
       │    │
       │    ├── project_history()
       │    ├── ToolRegistry::descriptors()
       │    ├── ChatProvider::chat()
       │    │    └── DeepSeekProvider
       │    │         ├── build_request_body()
       │    │         ├── POST /chat/completions
       │    │         └── parse SSE → ProviderEvent
       │    │
       │    ├── delta → events.jsonl → observer/TUI
       │    ├── handle_provider_events()
       │    ├── append assistant message
       │    └── 对每个 ToolCall
       │         ├── Tool::risk()
       │         ├── PermissionPolicy::decide()
       │         ├── ApprovalController
       │         ├── spawn_blocking(Tool::call)
       │         ├── materialize_output()
       │         └── append tool-result message
       │
       └── SessionFinished / AgentRun
```

### 3.3 状态层次

当前存在三个状态层：

1. 持久化会话状态：`session.json`、`messages.jsonl`、`events.jsonl`。
2. Agent 内存状态：本次运行的 `history`、turn 计数和 Provider 内部状态。
3. TUI 投影状态：从实时 `Event` 或 `events.jsonl` 构建 transcript。

目前 `resume` 只加载和展示历史 Session，不会恢复 Agent history，也不会继续原有 turn loop。

---

## 4. 设计合理之处

### 4.1 Agent loop 集中在统一 Runtime

核心循环集中在 `AgentRuntime`，CLI、TUI 和 Eval 没有维护独立实现：

- `crates/agent/src/lib.rs:130`
- `crates/cli/src/main.rs:137`
- `crates/tui/src/lib.rs:175`

这是当前最重要且最合理的架构决策。

### 4.2 Provider 不越界执行 Tool 或写 Session

DeepSeek adapter 负责：

- HTTP 请求
- Provider 消息转换
- SSE 解析
- ProviderEvent 生成

Tool 执行、权限判断、History 和存储都保留在 Agent Runtime：

- `crates/deepseek/src/lib.rs:56`
- `crates/agent/src/lib.rs:433`

边界清晰，便于未来替换 Provider。

### 4.3 流式 Event 与最终 Message 分离

Provider delta 先写入 `events.jsonl`，只有看到完成事件后才提交 assistant message：

- `crates/agent/src/lib.rs:304`
- `crates/agent/src/lib.rs:365`
- `crates/core/src/storage.rs:161`

正常的流中断不会直接把半截 assistant message 放入下一轮历史。

### 4.4 Tool 错误能够回灌模型

未知 Tool、执行错误、拒绝和取消都有 ToolResult 状态，而不是简单丢弃。模型在下一轮可以看到失败结果并调整行为。

### 4.5 文件 Tool 有基本 workspace 边界

文件读写实现会进行 canonicalize、绝对路径和父目录逃逸检查，能够阻止常见的 `..` 和 symlink workspace escape。

需要注意，这些保护不适用于 Bash。

### 4.6 大 Tool 输出采用 Artifact

超过 `max_output_bytes` 的完整输出会写入 Session artifact，History 只保留截断文本和 artifact 路径。这能控制 Prompt 膨胀，方向合理。

### 4.7 持久化 Schema 有 Golden Test

Message 和 Event 有完整 JSONL golden test，有助于防止协议无意漂移。

---

## 5. Agent 架构问题

## 5.1 P0：缺少明确的 Completion 状态机

Runtime 目前主要通过 `tool_calls.is_empty()` 判断任务是否成功，而不是根据 Provider 的终止原因决定。

相关代码：

- `crates/agent/src/lib.rs:237`
- `crates/agent/src/lib.rs:349`

### 触发场景

Provider 返回：

```rust
ProviderEvent::Done(StopReason::MaxTokens)
```

且该轮没有 Tool Call。

### 当前结果

Runtime 会：

1. 写入 `provider stopped at max tokens` 错误事件。
2. 仍返回正常 `TurnResult`。
3. 提交截断的 assistant message。
4. 因为 Tool Call 为空，写入 `SessionFinished(Succeeded)`。

同一个 Session 最终同时包含 Error 与 Succeeded，截断回答被当作完整结果。

### 建议

显式建模：

```text
TurnCompletion
├── EndTurn
├── ToolUse
├── MaxTokens
├── Cancelled
├── ProtocolError
└── ProviderFailure
```

只有合法的 `EndTurn` 才能自然进入任务成功。

---

## 5.2 P0：所有异常路径没有统一 Finalize

Provider 调用失败时通过 `?` 直接传播：

- `crates/agent/src/lib.rs:189`

Session 创建时写入：

```text
status = Running
```

- `crates/core/src/storage.rs:81`
- `crates/core/src/storage.rs:91`

但成功、失败和取消后都没有更新 `session.json.status` 或 `updated_at`。

### 后果

- API 认证失败可能没有 `SessionFinished`。
- SSE 解析失败可能没有终态事件。
- 成功 Session 的 metadata 仍然显示 `running`。
- TUI 会话列表长期显示错误状态。
- `events.jsonl` 与 `session.json` 成为互相冲突的状态源。

### 建议

所有返回路径统一经过：

```rust
finalize_session(outcome)
```

并保证：

1. 写入唯一终止事件。
2. 更新 `session.json.status`。
3. 更新 `updated_at`。
4. 再返回 `AgentRun` 或 `AgentError`。

---

## 5.3 P1：History 截断会破坏 Tool 消息配对

当前 History 按单条 Message 大小从尾部截断：

- `crates/agent/src/lib.rs:823`

但一个 Tool Turn 实际是不可拆分结构：

```text
assistant:
  tool_use A
  tool_use B

tool:
  result A

tool:
  result B
```

当前算法可能：

- 保留 ToolResult，却丢失对应的 assistant ToolUse。
- 保留 ToolUse，却丢失部分 ToolResult。
- 让发送给 Provider 的 History 以 `Role::Tool` 开头。
- 破坏 Provider 要求的 Tool Call 关联关系。

### 建议

按完整 Turn 分组投影：

```text
User Turn
Assistant Text + ToolUse[]
全部对应的 ToolResult[]
```

不能按单个 Message 任意截断。

---

## 5.4 P1：多 Tool Call 中途取消会留下孤立 ToolUse

Runtime 先一次性提交所有 ToolUse：

- `crates/agent/src/lib.rs:225`

随后串行执行工具：

- `crates/agent/src/lib.rs:252`

如果第一个工具执行后取消，第二个调用没有对应的 Cancelled ToolResult，History 会变成：

```text
assistant: tool_use A, tool_use B
tool: result A
```

这破坏了 ToolUse/ToolResult 一一对应的不变量。

### 建议

取消时遍历尚未执行的 Tool Call，并为每一个提交：

```text
ToolResultStatus::Cancelled
```

---

## 5.5 P2：Provider 抽象仍然偏向 OpenAI/DeepSeek

当前通用协议包含明显的 Chat Completions 假设：

- 独立 `Role::Tool`
- Tool arguments 保存为字符串
- `StopReason` 只有少数值
- Reasoning 只有文本，没有 opaque block/signature
- `ModelCapabilities` 没有接入 AgentRuntime

相关位置：

- `crates/provider/src/lib.rs`
- `crates/core/src/protocol.rs`
- `crates/agent/src/lib.rs:176`

如果未来增加 Claude Messages API，目前协议无法无损表达：

- assistant content blocks 原样回放
- thinking block 与 signature
- user-role `tool_result`
- `pause_turn`
- refusal
- stop sequence
- provider-specific opaque content

当前更准确的定位是：适配 OpenAI-compatible Chat Completions 的 Runtime，而不是完整的 provider-neutral Agent 协议。

---

## 6. Tool 设计问题

## 6.1 P0 阻断：所有内置 Tool Schema 都是非法 JSON

示例位置：

- `crates/tools/src/lib.rs:42`
- `crates/tools/src/lib.rs:67`
- `crates/tools/src/lib.rs:92`
- `crates/tools/src/lib.rs:257`

Schema 尾部多了引号，实际形式类似：

```text
{"type":"object", ..., "required":["path"]}""
```

DeepSeek 构造请求时才解析 Schema，因此默认内置 Tool 会在 HTTP 请求发出前导致 `InvalidRequest`。

### 为什么测试没有发现

- ToolRegistry 注册时不验证 Schema。
- DeepSeek 单元测试使用手写的合法 ToolSpec。
- 没有 `builtin_registry() → DeepSeek request body` 集成测试。

### 建议

- 将 `parameters()` 改为 `serde_json::Value` 或受验证的 Schema newtype。
- ToolRegistry 注册时立即验证 Schema。
- 增加所有 builtin descriptors 均可解析的契约测试。

---

## 6.2 P0 阻断：JSON Schema 与 Tool 实际输入协议完全不一致

Tool 向模型公开结构化参数，例如：

```json
{
  "path": "src/lib.rs"
}
```

但 Agent 将 Provider arguments 原样作为字符串传给：

```rust
Tool::call(input: &str, ...)
```

相关位置：

- `crates/agent/src/lib.rs:433`
- `crates/core/src/tools.rs:118`

工具实现则消费旧式裸字符串：

- Read 把整个 input 当路径：`crates/tools/src/lib.rs:51`
- Glob 把整个 input 当 pattern：`crates/tools/src/lib.rs:126`
- Bash 把整个 input 当 shell 命令：`crates/tools/src/lib.rs:266`
- Edit/Write 使用私有 delimiter 格式

### 触发结果

标准 Tool Call：

```json
{"path":"src/lib.rs"}
```

会尝试读取名为以下内容的文件：

```text
{"path":"src/lib.rs"}
```

标准 Bash Tool Call：

```json
{"command":"cargo test"}
```

会尝试将 JSON 对象文本本身交给 shell 执行。

### 建议

统一使用结构化输入：

```rust
fn risk(
    &self,
    input: &serde_json::Value,
) -> Result<ToolRisk, ToolError>;

fn call(
    &self,
    input: serde_json::Value,
    context: &ToolContext,
) -> Result<ToolOutput, ToolError>;
```

每个 Tool 用 serde struct 解析，例如：

```rust
#[derive(Deserialize)]
struct ReadInput {
    path: PathBuf,
    start_line: Option<usize>,
    end_line: Option<usize>,
}
```

`risk()` 和 `call()` 必须消费同一份解析结果，避免权限判断与真实执行采用不同解释。

---

## 6.3 P0 安全问题：Bash 权限边界可绕过

YOLO 模式中：

- Read、Write、Execute 自动 Allow
- Network 和 Destructive 要求 Ask

相关位置：

- `crates/core/src/tools.rs:43`

但 Bash 风险识别只依赖有限字符串黑名单，实际使用任意 shell 执行。

以下命令可能不会被识别为 Destructive：

```sh
git clean -fdx
find . -delete
git reset --hard
python -c 'import shutil; shutil.rmtree("src")'
sudo rm -rf .
```

以下网络命令也可能被识别为普通 Execute：

```sh
curl ...
wget ...
```

### 后果

- `allow_network=false` 没有形成实际约束。
- Bash 可以读取和写入 workspace 外路径。
- YOLO 可以自动执行未被字符串规则识别的破坏性行为。
- `ToolRisk::Network` 和配置中的安全承诺与实际执行不一致。

### 建议

在真正的 sandbox 完成前：

1. Bash 在 YOLO 下仍然 Ask。
2. 明确 Bash 不受 workspace 文件边界保护。
3. 将 `allow_network` 真正连接到执行环境。
4. 使用隔离进程或 sandbox，不依赖危险命令黑名单。

---

## 6.4 P1：Shell 大输出可能造成 Pipe 死锁

Shell 将 stdout/stderr 设为 pipe，但在进程退出后才读取。

如果子进程输出超过 OS pipe buffer：

1. 子进程阻塞在写 pipe。
2. 父进程等待子进程退出。
3. 子进程无法退出。
4. 最终被错误报告为 timeout。

### 建议

- 并发读取 stdout 和 stderr。
- 在读取过程中实施输出上限。
- 不要等进程完成后才一次性收集输出。

---

## 6.5 P1：Shell Timeout 不保证终止进程树

当前 timeout 只 kill 顶层 shell。其子进程或后台进程可能继续运行、写文件、占用端口或消耗资源。

这会导致 Runtime 已经记录 Tool 执行失败，但实际副作用仍在继续。

### 建议

- Unix 使用独立进程组并终止整个进程组。
- Windows 使用 Job Object 等等价机制。
- Cancel 与 Timeout 应共享同一进程终止路径。

---

## 6.6 P2：Tool API 信息不足

当前 `ToolOutput` 只有：

```text
stdout
stderr
status
```

缺少：

- exit code
- duration
- timeout
- signal
- truncated
- artifact
- retryable
- structured content

`ToolError` 也只有 message，无法区分：

```text
InvalidInput
PermissionDenied
Io
Timeout
Cancelled
Internal
```

这会限制 Runtime 策略、UI 展示以及模型修正参数。

### 建议

引入结构化错误和输出元数据，同时避免让模型从普通文本中推断 Tool 状态。

---

## 6.7 P2：Legacy Tool 不应全部对模型公开

当前 Registry 同时注册并 advertise 7 个新工具和 7 个 legacy 工具：

- `crates/tools/src/lib.rs:12`

这会：

- 增大 Prompt 和 Schema 体积。
- 增加模型选择歧义。
- 使新旧输入协议继续共存。

兼容别名可以保留在 dispatch 层，但不应全部通过 `advertised_tools()` 提供给新模型请求。

---

## 7. 流式消息传递问题

## 7.1 P0：HTTP Chunk 跨 UTF-8 字符时会损坏内容

当前每个 reqwest chunk 独立执行：

```rust
String::from_utf8_lossy(&chunk)
```

- `crates/deepseek/src/lib.rs:67`

HTTP chunk 可以在任意字节处切分，包括中文、emoji 或 Tool JSON Unicode 字符的多字节序列中间。

### 后果

- assistant 文本乱码
- reasoning 乱码
- Tool JSON arguments 损坏
- SSE JSON 解析失败

### 建议

- 保持原始字节缓冲。
- 先按 SSE framing 提取完整数据，再进行 UTF-8 解码。
- 或使用增量 UTF-8 decoder，保留跨 chunk 的未完成字节。

---

## 7.2 P0：部分流输出后的自动重试会产生重复或幽灵 Delta

每个 delta 会立即写入 `events.jsonl` 并通知 TUI：

- `crates/agent/src/lib.rs:402`

如果第一次请求已经输出 `hel` 后发生可重试错误，第二次完整输出 `hello`：

- `messages.jsonl` 最终只有 `hello`
- `events.jsonl` 包含 `hel` 和 `hello`
- TUI 可能显示 `helhello`

失败 attempt 的 delta 已经对外可见，却没有 attempt ID、撤销事件或去重依据。

### 建议

可选方案：

1. 一旦已经对外发布 delta，就不再透明重试。
2. 为事件增加 `request_id + attempt`，失败时发送 `AttemptInvalidated`。
3. Provider attempt 先写临时流，成功后再提升为正式 transcript。

---

## 7.3 P1：取消不能中断正在进行的 Provider 或 Tool

TUI 提交任务后直接 await 整个 Runtime：

- `crates/tui/src/lib.rs:105`
- `crates/tui/src/lib.rs:130`
- `crates/tui/src/lib.rs:175`

此时 input loop 不再读取终端按键，所以用户按 Esc 或 Ctrl-C 无法触发实时取消。

Agent 的取消只在以下边界轮询：

- Provider 请求前后
- Tool 与 Tool 之间

它不能中断：

- 正在等待的 HTTP/SSE
- 正在运行的 Bash
- `spawn_blocking` 内的 Tool
- 等待中的审批

### 建议

引入贯穿以下链路的 cancellation token：

```text
TUI → Agent → Provider → Tool
```

同时让 TUI input loop 与 Agent task 并发运行。

---

## 7.4 P1：流式链路存在同步背压与 O(n²) 存储

每个 token delta 当前都会同步执行：

```text
读取完整 events.jsonl 计算 sequence
追加 Event
调用 observer
创建或刷新 Terminal
继续读取网络
```

`next_sequence()` 每次完整读取日志：

- `crates/core/src/storage.rs:301`

Delta 持久化位于 Provider callback：

- `crates/agent/src/lib.rs:416`

TUI 每个 delta 整屏 redraw：

- `crates/tui/src/lib.rs:208`

### 后果

- Event sequence 计算趋近 O(n²)。
- TUI 重绘成本随 transcript 增长。
- 慢磁盘和慢终端直接反压 HTTP stream。
- 长回答可能显著降低吞吐，甚至导致网络读取超时。

此外，delta 的持久化错误被忽略：

```rust
let _ = append_event(...)
```

- `crates/agent/src/lib.rs:419`

磁盘满时 UI 可能显示事件，但回放中永久缺失，破坏“先持久化再通知”的不变量。

### 建议

- Session 内维护 sequence counter。
- 使用有界 channel 解耦 Provider、Storage、State reducer 和 TUI。
- 合并相邻 delta。
- TUI 固定帧率刷新，而不是每 token 刷新。
- 持久化失败必须进入明确错误和 Session finalize 路径。

---

## 7.5 P1：`[DONE]` 可能静默丢失 Tool Call

Tool Call 只在以下 finish reason 时 flush：

```text
finish_reason = tool_calls
```

- `crates/deepseek/src/lib.rs:188`

但 `[DONE]` 会直接补成：

```text
Done(EndTurn)
```

- `crates/deepseek/src/lib.rs:150`

如果兼容 Provider 已发送 Tool Call delta，却没有正确发送 `finish_reason=tool_calls`，残留 builder 会被静默丢弃，Agent 会把该轮当普通成功回答。

### 建议

`[DONE]` 只应代表传输结束。结束时必须检查：

- 是否已经收到明确 finish reason。
- 是否仍有未完成 Tool Call。
- 是否存在残留或非法 parser 状态。

不能无条件推断 EndTurn。

---

## 7.6 P2：TUI 没有正确累计流式文本

当前每个 delta 都成为独立 transcript line，渲染过程中还会压缩原始空白。

可能呈现为：

```text
assistant: Hel
assistant: lo
assistant:
assistant: world
```

代码块缩进、连续空格和换行也可能丢失。

### 建议

使用流状态 reducer：

```text
AssistantStreaming {
    request_id,
    attempt,
    accumulated_text
}
```

相邻 delta 应追加到同一个 block，而不是生成新的 transcript line。

---

## 7.7 P2：Reasoning 没有进入已提交消息

`ReasoningDelta` 只进入 Event log，不累计到最终 assistant message。定义好的 `ModelCapabilities` 也没有参与 Agent loop。

这意味着：

- 下一轮 Provider 看不到上一轮 reasoning block。
- 无法满足某些 Provider 对 tool turn reasoning 回放的要求。
- Provider capability 目前只是未接入的平行抽象。

如果未来支持需要 opaque reasoning 或 signature 的 Provider，必须重新定义内容块的 round-trip 语义。

---

## 8. 配置与产品语义问题

### 8.1 多个配置字段没有进入运行路径

包括：

- `allow_network`
- `shell_timeout_secs`
- `reasoning_effort`

字段虽然可以解析，但没有改变实际执行行为。尤其 `allow_network=false` 会给用户造成错误的安全预期。

### 8.2 默认 System Prompt 没有接入 Runtime

仓库存在 `prompts/system_default.md`，但 Runtime 初始 History 只有 User Message，没有将该 Prompt 放入请求。

真实 Provider 不会得到仓库附带的 coding-agent 行为、Tool 使用方式和完成标准。

### 8.3 `resume` 名称与实际能力不一致

当前 `resume` 只读取和展示历史，不会：

- 从 `messages.jsonl` 重建 Agent history
- 继续原 Session
- 恢复 pending Tool Call
- 恢复审批状态

应明确命名为 replay，或真正实现 Session continuation。

---

## 9. 测试盲区

### 9.1 Tool 协议

1. 所有 builtin Tool Schema 均可解析。
2. 每个 Tool 按公开 JSON Schema 输入执行。
3. required、类型错误、未知字段和整数边界。
4. `builtin_registry → DeepSeek request body` 集成测试。
5. DeepSeek SSE Tool arguments → AgentRuntime → 真实 Tool 的端到端测试。
6. 多 Tool Call 的 ID、顺序和结果配对。
7. 多 Tool 中途取消后，剩余调用获得 Cancelled Result。

### 9.2 Agent 状态机

8. `MaxTokens` 最终不能是 Succeeded。
9. Provider 最终失败后必须有唯一终止事件。
10. 成功、失败、取消后更新 `session.json`。
11. `Done(ToolUse)` 但没有 Tool Call 的协议错误。
12. `Done(EndTurn)` 同时包含 Tool Call 的处理。
13. History projection 不拆开 Tool Turn。
14. 单条消息超过 Prompt budget 的行为。

### 9.3 SSE 与 Streaming

15. UTF-8 多字节字符跨 reqwest chunk。
16. `\r\n`、最后无换行、一个 Event 跨多个 chunk。
17. 多行 `data:`、heartbeat 和 comment。
18. 已输出 delta 后连接失败并重试。
19. 未知 finish reason。
20. Tool Call delta 后直接 `[DONE]`。
21. 多 choice 和 choice 交错。

### 9.4 取消和进程

22. Provider 正在 streaming 时真实取消。
23. Tool 正在执行时真实取消。
24. TUI 运行期间按 Esc/Ctrl-C。
25. Shell timeout 后整个进程树退出。
26. 大 stdout/stderr 不发生 pipe 死锁。

### 9.5 安全

27. Bash 风险绕过矩阵：

```text
git clean -fdx
find . -delete
git reset --hard
python/shell interpreter indirection
curl/wget
command substitution
redirection outside workspace
```

28. `allow_network=false` 的执行级测试。
29. Provider-controlled call ID 的 artifact 路径净化。
30. Symlink TOCTOU 与 workspace 边界。

### 9.6 TUI 和回放

31. 多 delta 合并为一个 assistant/reasoning block。
32. Markdown、代码缩进、空行和 CJK 宽字符。
33. 实时渲染与重启回放结果一致。
34. Session 状态与历史列表排序一致。

---

## 10. 建议改造优先级

## 10.1 P0：恢复真实 Tool Calling

1. 修正所有非法 Tool Schema。
2. ToolRegistry 注册时验证 Schema。
3. Tool input 改为结构化 JSON。
4. 每个 Tool 使用 serde struct 解析输入。
5. 隔离 legacy string adapter，不再向模型公开全部 legacy Tool。
6. 增加真实集成链路测试：

```text
builtin_registry
→ ChatRequest
→ DeepSeek request body
→ SSE tool arguments
→ AgentRuntime
→ Read/Write/Bash
→ ToolResult 回灌
```

## 10.2 P0：修复 Completion 与 UTF-8

1. `MaxTokens` 不能进入 Succeeded。
2. 未知 finish reason 和流提前结束应为明确失败。
3. 使用字节缓冲或增量 decoder 修复跨 chunk UTF-8。
4. `[DONE]` 不再无条件推断 EndTurn。

## 10.3 P0：建立真实安全边界

1. Bash 在无 sandbox 时不应被 YOLO 静默允许。
2. 真正落实 network policy。
3. 明确 Bash 不具备 workspace isolation。
4. Tool input、call ID 和 artifact 路径都按不可信输入处理。
5. 不再依赖命令字符串黑名单作为核心安全机制。

## 10.4 P1：建立可靠 Runtime 状态机

1. 所有路径统一 finalize Session。
2. 更新 `session.json.status/updated_at`。
3. ToolUse/ToolResult 形成不可破坏的不变量。
4. History 按完整 Tool Turn 截断。
5. Provider retry 引入 attempt 事务语义。
6. Completion、Provider failure、Protocol failure 和 Cancellation 分开建模。

## 10.5 P1：实现端到端取消

1. 引入 cancellation token。
2. TUI 输入与 Agent task 并发。
3. Provider streaming 监听 token。
4. Approval 等待监听 token。
5. ToolContext 携带 token。
6. Shell cancel/timeout 终止整个进程组。

## 10.6 P1：修复 Shell 执行

1. 并发读取 stdout/stderr。
2. 读取过程中实施输出上限。
3. 保留 exit code、signal、duration、timeout 等元数据。
4. ToolError 使用结构化分类。

## 10.7 P2：重构流式事件链路

建议目标架构：

```text
Provider Stream
      │
      ▼
bounded channel
      │
      ▼
Agent State Reducer
  ├── Streaming Attempt State
  ├── Final Message Commit
  ├── Tool State
  └── Session State
      │
      ├── Storage Writer
      └── TUI Renderer
```

该结构可以统一解决：

- 背压
- attempt 区分
- delta 合并
- 固定帧率渲染
- 取消
- 实时状态与回放一致性

## 10.8 P2：收敛 Provider 抽象

如果目标包括多 Provider，应统一或重新定义：

- 完整 assistant content blocks
- opaque provider blocks round-trip
- structured ToolResult
- 更完整 StopReason
- system/request options
- Provider capabilities
- reasoning、pause、resume 和 refusal 语义

当前未接入的 `Provider` / `ModelCapabilities` 平行抽象应真正接入 Runtime，或者暂时删除，避免制造已经 capability-aware 的错觉。

---

## 11. 建议实施顺序

建议按照以下阶段推进：

### 第一阶段：正确性闭环

- 修复 Schema。
- 统一 JSON Tool input。
- 修复 MaxTokens。
- 修复 UTF-8。
- 增加真实 DeepSeek Tool Calling 集成测试。

### 第二阶段：状态一致性

- 引入明确 Turn Completion。
- 统一 Session finalize。
- 更新 Session metadata。
- 保证 ToolUse/ToolResult 配对。
- 按 Tool Turn 投影 History。

### 第三阶段：取消与进程安全

- 引入 cancellation token。
- TUI 与 Runtime 并发。
- Shell process group。
- stdout/stderr 异步读取。
- Bash 安全策略与 network policy。

### 第四阶段：Streaming 架构

- bounded channel。
- attempt ID。
- delta reducer。
- sequence counter。
- 固定帧率 TUI。
- 实时与回放一致性测试。

### 第五阶段：Provider 扩展性

- 收敛 Provider traits。
- 接入 capabilities。
- 扩展 content block 与 StopReason。
- 增加第二个真实 Provider 或协议 fixture 套件。

---

## 12. 最终评价

当前代码的宏观拆分是合理的，Agent Runtime、Provider adapter、Tool、Storage 和 TUI 的职责边界值得继续保留，不需要推倒重来。

真正的问题集中在跨层契约没有闭环：

- Tool Schema 与 Tool 实际输入不一致。
- Provider Completion 与 Agent Outcome 不一致。
- Event 流与最终 Message 在重试场景下不一致。
- Session metadata 与终态 Event 不一致。
- PermissionPolicy 与 Bash 实际安全边界不一致。
- TUI 所表达的取消能力与 Runtime 实际可取消能力不一致。

这些契约目前主要依靠约定，而没有被类型、状态机和集成测试强制保证。

在 P0 问题解决前，不建议将当前版本定义为“真实 Tool Calling 与可靠流式 Agent 已完成”。建议先恢复 Tool 协议和流式正确性，再处理取消、重试、安全边界与 Provider 扩展性。

---

# 13. 不向后兼容重构方案

本轮重构目标不是在现有实现上继续补丁式兼容，而是把已经暴露的问题收敛为新的 v1 Agent Runtime 契约。既然不需要向后兼容，应主动删除旧协议、旧工具名、旧 session 续跑语义和旧字符串 Tool 输入，避免新的实现继续背负双协议复杂度。

## 13.1 重构原则

1. 只保留一个公开协议：Provider 只能看到 canonical Tool、结构化 JSON input、明确 StopReason 和结构化 Message/Event。
2. 只保留一个状态源：`session.json` 是 session metadata 状态源，`events.jsonl` 是事件事实日志，`messages.jsonl` 是模型历史事实日志，三者必须由 Runtime 统一提交。
3. 所有跨层边界都用类型表达：Provider payload、Tool schema、Tool input、Tool output、Tool error、Completion、Cancellation、Session finalize 都不能依赖裸字符串约定。
4. 所有终止路径都必须 finalize：成功、失败、取消、Provider 错误、协议错误、Tool 错误、存储错误都要进入唯一终态。
5. 安全能力不能靠提示词承诺：没有真实 sandbox 前，Bash 不能在 YOLO 下静默执行，`allow_network` 必须连接到执行层或从配置中移除。
6. 实时 UI 只是 Runtime 状态投影：TUI 不拥有独立 Agent 状态机，实时渲染和 session replay 必须从同一事件语义还原出一致 transcript。

## 13.2 目标架构

重构后的目标链路如下：

```text
CLI / TUI / Eval
      │
      ▼
AgentRuntime
  ├── RuntimeStateMachine
  │   ├── TurnCompletion
  │   ├── ToolTurn pairing
  │   ├── Attempt transaction
  │   ├── Cancellation
  │   └── Session finalize
  │
  ├── ProviderClient
  │   ├── typed ChatRequest
  │   ├── typed ToolSpec
  │   ├── byte-safe SSE parser
  │   └── ProviderEvent stream
  │
  ├── ToolExecutor
  │   ├── canonical ToolRegistry
  │   ├── serde_json input
  │   ├── typed output/error
  │   └── process sandbox policy
  │
  ├── StorageWriter
  │   ├── append-only messages/events
  │   ├── monotonic sequence counter
  │   └── atomic session metadata update
  │
  └── EventReducer
      ├── streaming delta coalescing
      ├── attempt visibility
      ├── replay projection
      └── TUI projection
```

目标依赖方向保持：

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
      agent
        ^
        |
   cli / tui / eval
```

## 13.3 核心数据模型目标

### Tool 协议

删除所有 legacy concrete tools：

- 删除 `read_file`
- 删除 `apply_patch`
- 删除 `write_file`
- 删除 `search`
- 删除 `shell`
- 删除 `git_diff`
- 删除 `run_tests`

Provider 可见工具只保留：

- `Read`
- `Edit`
- `Write`
- `Glob`
- `Grep`
- `ListFiles`
- `Bash`

Tool trait 改为结构化输入：

```rust
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> &'static serde_json::Value;
    fn risk(&self, input: &serde_json::Value) -> Result<ToolRisk, ToolError>;
    fn call(
        &self,
        input: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolOutput, ToolError>;
}
```

每个 Tool 必须定义对应的 serde input struct。`risk()` 与 `call()` 必须解析同一类型，避免权限判断和真实执行解释不同。

### Provider 协议

`ToolSpec.parameters` 改为 `serde_json::Value` 或验证过的 `JsonSchema` newtype。`ToolCall.input` 改为 `serde_json::Value`。DeepSeek adapter 负责把 Provider 原始 arguments 字符串解析为 JSON Value，解析失败必须变成 `ProviderError::InvalidRequest` 或协议错误事件，不能把坏字符串交给 Tool。

`StopReason` 至少扩展为：

```rust
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    Refusal,
    Cancelled,
    Unknown(String),
}
```

Agent 内部再映射为：

```rust
pub enum TurnCompletion {
    EndTurn,
    ToolUse,
    MaxTokens,
    Cancelled,
    ProtocolError(String),
    ProviderFailure(String),
}
```

只有 `TurnCompletion::EndTurn` 且无 ToolCall 时可以成功结束任务。`ToolUse` 必须包含至少一个 ToolCall。`MaxTokens`、`ProtocolError`、`ProviderFailure` 都不能写入 `Succeeded`。

### Message 与 Event

`ContentBlock::ToolUse.input` 改为 `serde_json::Value`，`ToolResult` 携带结构化状态与文本内容。Reasoning 是否回放给下一轮由 Provider capability 决定，但协议必须能表达。

Event 增加 request/attempt 维度：

```rust
ModelRequestStarted { request_id, attempt, model }
AssistantDelta { request_id, attempt, text }
ReasoningDelta { request_id, attempt, text }
AttemptFailed { request_id, attempt, retryable, message }
AttemptCommitted { request_id, attempt }
AttemptInvalidated { request_id, attempt, reason }
```

已经对外发布 delta 后，不允许透明重试生成重复 transcript。要么禁止重试，要么通过 attempt 事件显式 invalidation。

### Tool 输出与错误

`ToolOutput` 增加执行元数据：

```rust
pub struct ToolOutput {
    pub stdout: String,
    pub stderr: String,
    pub status: ToolExitStatus,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub truncated: bool,
    pub artifact: Option<String>,
}
```

`ToolError` 改为结构化分类：

```rust
pub enum ToolErrorKind {
    InvalidInput,
    PermissionDenied,
    Io,
    Timeout,
    Cancelled,
    ProcessFailed,
    Internal,
}
```

Runtime 根据错误类型决定 ToolResult status、是否继续下一轮、是否 finalize failed。

## 13.4 Runtime 状态机目标

AgentRuntime 必须从“过程式循环”改成显式状态机：

```text
SessionRunning
  ├── TurnStarted
  │   ├── ProviderStreaming
  │   ├── TurnCompleted(EndTurn)
  │   ├── TurnCompleted(ToolUse)
  │   └── TurnFailed(...)
  │
  ├── ToolTurnStarted
  │   ├── ToolApprovalPending
  │   ├── ToolRunning
  │   ├── ToolFinished
  │   └── ToolCancelled
  │
  └── SessionFinalized(Succeeded | Failed | Cancelled)
```

强制不变量：

1. 每个 session 只能有一个 `SessionFinished`。
2. `SessionFinished` 与 `session.json.status` 必须一致。
3. 每个 assistant ToolUse 必须有且只有一个 ToolResult。
4. 多 ToolCall 中途取消时，尚未执行的 ToolCall 必须写入 `Cancelled` ToolResult。
5. History projection 只能按完整 turn 截断，不能留下孤立 ToolUse 或 ToolResult。
6. Provider stream 没有明确 finish reason 时必须失败，不能推断成功。
7. Storage 写入失败必须进入 finalize failed，不能先通知 UI 再静默丢事件。

## 13.5 Bash 与安全目标

在没有真正 sandbox 前，Bash 策略改为保守默认：

1. `Bash` 在 `Yolo` 模式下默认 `Ask`，除非命令被显式识别为允许列表中的只读/测试命令。
2. `allow_network=false` 时，Bash 执行环境必须阻断网络；如果当前平台不能阻断，则所有疑似网络命令必须 `Deny` 或 `Ask`，并在配置加载时给出明确降级说明。
3. Bash 必须使用独立进程组。取消或超时时终止整个进程组。
4. stdout/stderr 必须并发读取，并在读取过程中执行输出上限。
5. 文件 Tool 的 workspace guard 保留并加强：call id、artifact path、symlink、写入 parent canonicalization 都按不可信输入处理。

## 13.6 Streaming 与 TUI 目标

DeepSeek SSE parser 必须从 byte stream 开始处理：

```text
reqwest bytes
  → byte buffer
  → SSE frame parser
  → UTF-8 decode complete data field
  → provider JSON parse
  → ProviderEvent
```

TUI 与 Runtime 必须并发运行：

```text
keyboard/input task ─┐
approval/cancel chan ├── AgentRuntime
runtime event chan ──┘
        │
        ▼
TUI reducer → fixed-rate render
```

TUI reducer 将相邻 delta 合并为同一个 assistant/reasoning block。实时 transcript 与从 `events.jsonl` replay 出来的 transcript 必须一致。

## 13.7 Session 与命令语义目标

不兼容前提下，建议重命名或重定义命令：

- `replay <session_id>`：只回放历史，不继续运行。
- `continue <session_id>`：从 `messages.jsonl` 重建 history 并继续同一 session。
- 如果短期不实现真正 continuation，则删除或隐藏 `resume`，避免误导。

旧 session schema 不需要迁移。重构后可以要求从新 schema 创建 session；旧 `.flash/sessions` 只作为人工排查资料，不承诺可 replay。

---

# 14. 分阶段改造计划

## 阶段 1：协议归一与 Tool Calling 可用

目标：让真实 Provider Tool Calling 从 schema 到 ToolResult 回灌形成闭环。

改造项：

1. 删除 legacy tool 注册和实现。
2. 将 Tool schema 改为 `serde_json::Value` / `JsonSchema` newtype。
3. ToolRegistry 注册时验证 schema 合法性、工具名唯一性和 Provider 可见性。
4. `Tool::risk` / `Tool::call` 改为消费 JSON Value。
5. `Read/Edit/Write/Glob/Grep/ListFiles/Bash` 全部使用 serde input struct。
6. `Read` 实现 `start_line/end_line`，并定义越界行为。
7. `Edit` 改为 `{ path, find, replace }`，删除 delimiter 私有格式。
8. `Write` 改为 `{ path, content }`，删除 `---CONTENT---` 私有格式。
9. DeepSeek request body 从 typed `ToolSpec` 生成，并增加 builtin registry 集成测试。

退出标准：

- DeepSeek request body 能包含 7 个且仅 7 个 canonical tools。
- 所有 builtin schema 可被 serde_json 解析并通过注册验证。
- 标准 JSON ToolCall `{"path":"README.md"}` 能真实触发 `Read`。
- 标准 JSON ToolCall `{"command":"cargo test"}` 不会把 JSON 文本交给 shell。

## 阶段 2：Completion 与 Session Finalize

目标：消除 “Error + Succeeded”、“running session 永不结束” 和截断回答误成功。

改造项：

1. 引入 `TurnCompletion`。
2. `handle_provider_events` 返回完整 `TurnOutcome`，而不是仅返回 text + tool_calls。
3. `MaxTokens`、未知 finish reason、流提前结束、ToolUse 无 ToolCall 都进入协议失败。
4. 实现 `finalize_session(outcome)`，统一写 `SessionFinished`、更新 `session.json.status/updated_at`。
5. 所有 `?` 传播路径改为经过 finalize 后再返回。
6. 确保每个 session 只有一个终态事件。

退出标准：

- Provider authentication failure 会留下 `SessionFinished(Failed)` 和 `session.json.status = failed`。
- `Done(MaxTokens)` 不可能得到 `Succeeded`。
- 正常成功 session 的 `session.json.status = succeeded`。
- 取消 session 的 `session.json.status = cancelled`。

## 阶段 3：ToolTurn 不变量与 History Projection

目标：Provider history 永远满足 ToolUse/ToolResult 配对契约。

改造项：

1. 将 history projection 从单条 Message 截断改为 turn 分组。
2. 定义 `ConversationTurn`：

```text
User
Assistant(Text/Reasoning/ToolUse[])
ToolResult[]
```

3. 多 ToolCall 执行时维护 pending set。
4. 中途取消时为所有 pending ToolCall 写入 `Cancelled` ToolResult。
5. Provider 请求前执行 history invariant check。

退出标准：

- 任何 Provider request 不以 `Role::Tool` 开头。
- 任何 ToolResult 都能在当前 projected history 中找到对应 ToolUse。
- 任何 ToolUse 在进入下一轮前都有 ToolResult。
- prompt budget 很小时也只丢完整 turn，不拆 ToolTurn。

## 阶段 4：Byte-safe SSE 与 Retry 事务

目标：修复 UTF-8 损坏、DONE 误判、重试重复 delta。

改造项：

1. DeepSeek SSE parser 改为 byte buffer + frame parser。
2. 支持跨 chunk UTF-8、多行 `data:`、`\r\n`、comment/heartbeat、最后无换行。
3. `[DONE]` 只表示传输结束，不直接推断 EndTurn。
4. finish reason 与 tool call builder 状态必须一致。
5. 引入 request id + attempt id。
6. 已发布 delta 后禁止透明重试，或实现 `AttemptInvalidated`。

退出标准：

- 中文/emoji 跨 HTTP chunk 不乱码。
- Tool arguments 跨 chunk 不损坏。
- ToolCall delta 后直接 `[DONE]` 会失败，不会静默成功。
- 可重试错误不会在 TUI/replay 中产生重复 transcript。

## 阶段 5：取消、进程与安全边界

目标：用户取消能真实中断 Provider、Approval、Tool 和 Bash 子进程树。

改造项：

1. 引入 cancellation token，并贯穿 TUI、Agent、Provider、ToolContext、Approval。
2. TUI input loop 与 Agent runtime task 并发。
3. Provider streaming select cancellation token。
4. Tool call 支持取消信号。
5. Bash 使用进程组，timeout/cancel 终止整个进程组。
6. stdout/stderr 并发读取并实施输出上限。
7. Bash 在 YOLO 下默认 Ask，直到 sandbox/network policy 真实落地。
8. `allow_network` 接入执行层，无法保证时配置层报出明确限制。

退出标准：

- TUI 运行长 Provider stream 时按 Esc/Ctrl-C 能在限定时间内取消。
- 长 Bash 命令取消后无残留子进程。
- 大 stdout/stderr 不会 pipe 死锁。
- `allow_network=false` 下网络命令不会静默执行。

## 阶段 6：Streaming Event Pipeline 与 TUI Replay 一致

目标：实时事件、持久化事件和 UI 投影一致，并且不会因每 token 同步写入拖垮流。

改造项：

1. 引入 bounded event channel。
2. StorageWriter 维护 session 内 sequence counter，删除每次 append 前读全量日志。
3. EventReducer 合并相邻 assistant/reasoning delta。
4. TUI 固定帧率刷新，而不是每 token redraw。
5. 持久化成功后再通知 UI；持久化失败进入 finalize failed。
6. replay 使用同一个 reducer。

退出标准：

- 长流式回答不会出现 O(n²) sequence 写入。
- 实时 TUI 与重启后 replay 的 transcript 一致。
- Markdown 代码块、空行、缩进、CJK 宽字符显示稳定。
- 磁盘写入失败不会造成 UI 看见但日志缺失的事件。

## 阶段 7：Provider Capability 收敛

目标：明确当前是 DeepSeek/OpenAI-compatible Runtime，还是 provider-neutral Runtime。

建议选择：

1. 短期定位为 OpenAI-compatible Runtime：删除未接入的 capability 假象，只保留当前真实使用字段。
2. 中期再扩展 provider-neutral：引入 opaque content block、reasoning signature、pause/refusal/tool_result role 差异。

若选择 provider-neutral，改造项：

- `ContentBlock` 支持 provider opaque block round-trip。
- `ChatRequest` 支持 system message、request options、reasoning options。
- `ProviderEvent` 支持 refusal、pause、stop_sequence、unknown finish reason。
- Agent 根据 `ModelCapabilities` 决定 reasoning/tool result 回放策略。

退出标准：

- Runtime 中不存在“定义了但完全未接入”的 capability 字段。
- 新 Provider 接入时不需要修改 ToolExecutor 或 StorageWriter。
- Provider-specific DTO 不泄漏到 `core`。

---

# 15. 重构后的完整目标

重构完成后，Flash Code Agent 应达到以下目标：

1. 真实 Tool Calling 可用：DeepSeek 返回的标准 function/tool call 能被 Runtime 正确解析、执行并回灌模型。
2. Tool 协议唯一：Provider 只看到 7 个 canonical tools，不存在 legacy 名称和旧字符串输入。
3. Tool schema 可信：所有工具 schema 在注册时验证，无法把非法 schema 送进 Provider request。
4. Tool 输入结构化：权限判断和执行基于同一个 serde input 类型。
5. Completion 正确：Provider stop reason 与 Agent outcome 有明确映射，截断、协议错误和 Provider 错误不会被标记成功。
6. Session 状态一致：`session.json`、`events.jsonl`、`messages.jsonl` 对终态和时间戳表达一致。
7. ToolTurn 配对完整：任何历史投影都不会出现孤立 ToolUse 或孤立 ToolResult。
8. Streaming 字节安全：HTTP chunk 任意切分都不会破坏 UTF-8、SSE frame 或 Tool JSON arguments。
9. Retry 语义明确：已经展示的 delta 不会在重试后产生重复或幽灵内容。
10. 取消端到端有效：用户取消能中断 Provider stream、审批等待、Tool 执行和 Bash 子进程树。
11. Bash 安全边界诚实：无 sandbox 时不承诺 sandbox，YOLO 不静默放行任意 Bash，network policy 有执行级约束。
12. Shell 执行可靠：大输出不死锁，timeout/cancel 不遗留进程树，输出元数据足够 UI 和模型判断。
13. 流式 UI 稳定：TUI 固定帧率刷新，delta 合并为自然 transcript，实时显示和 replay 一致。
14. Storage 可恢复：append-only 日志保持一致性，写入失败不会形成半成功状态。
15. Provider 抽象诚实：要么明确 DeepSeek/OpenAI-compatible，要么完整支持 provider-neutral content round-trip，不保留未接入的能力假象。
16. 配置字段真实生效：`allow_network`、`shell_timeout_secs`、`reasoning_effort` 要么接入运行路径，要么从配置中移除。
17. 命令语义清楚：`replay` 只回放，`continue` 才续跑；不实现续跑时不保留误导性的 `resume`。
18. 测试覆盖跨层契约：P0/P1 问题都有可复现回归测试，而不是只依赖 mock provider 和单层单元测试。

---

# 16. 总体验收标准

以下验收标准全部满足后，才能认为本轮不兼容重构完成。

## 16.1 协议与 Tool

- `builtin_registry().descriptors()` 返回 7 个且仅 7 个工具：`Read/Edit/Write/Glob/Grep/ListFiles/Bash`。
- 所有 descriptor 的 `parameters` 是合法 JSON Schema object。
- Registry 注册非法 schema 会失败。
- Provider request body 中不出现 legacy 工具名。
- `ToolCall.input` 在 provider 层已被解析为 JSON Value。
- `Read { path, start_line, end_line }` 支持完整读取和区间读取。
- `Edit { path, find, replace }` 能精准替换；找不到 `find` 时返回 `InvalidInput` 或可理解的 Tool error。
- `Write { path, content }` 不再使用 delimiter 协议。
- `Bash { command, timeout_secs }` 执行的是 `command` 字段，而不是整个 JSON 文本。

## 16.2 Runtime 与 Session

- `EndTurn + no tool calls` 才能自然 `Succeeded`。
- `ToolUse + tool calls` 会进入 Tool 执行和下一轮模型请求。
- `ToolUse + no tool calls` 是协议错误。
- `EndTurn + tool calls` 是协议错误或按明确定义修正，不能静默成功。
- `MaxTokens` 结果为 `Failed` 或需要用户继续的明确状态，不能是 `Succeeded`。
- Provider error、storage error、protocol error、cancel 都会写入唯一 `SessionFinished`。
- `session.json.status` 与最终 `SessionFinished.outcome` 一致。
- `updated_at` 在每次 message/event/finalize 后更新，至少 finalize 后必须更新。
- 成功、失败、取消路径都通过同一个 finalize API。

## 16.3 History 与 ToolTurn

- 每个 projected history 都通过 ToolTurn invariant check。
- prompt 截断只按完整 turn 丢弃。
- 多 ToolCall 顺序稳定，ToolResult 按 call_id 正确对应。
- 任一 Tool 被拒绝、失败或取消时，模型下一轮能看到结构化 ToolResult。
- 多 ToolCall 中途取消时，所有未执行 ToolCall 都有 `Cancelled` ToolResult。

## 16.4 Streaming 与 Provider

- SSE parser 通过跨 chunk UTF-8 测试。
- SSE parser 通过跨 chunk JSON arguments 测试。
- 支持 `\n`、`\r\n`、多行 `data:`、comment、heartbeat 和最后无换行。
- `[DONE]` 不会覆盖未完成 tool builder 状态。
- 未知 finish reason 被显式记录，不会推断成功。
- 已经发布 delta 的 attempt 失败后，不会在 transcript 中留下不可解释重复内容。
- `UsageRecorded` 与 request/attempt 关联清楚。

## 16.5 取消、安全与进程

- TUI 中 Provider streaming 期间可取消。
- Approval pending 期间可取消。
- Tool running 期间可取消。
- Bash timeout 会终止整个进程组。
- Bash cancel 会终止整个进程组。
- stdout/stderr 大输出不会阻塞进程退出。
- 输出超过限制时 `truncated = true`，完整输出按策略写 artifact。
- `allow_network=false` 有执行级测试证明网络命令不会被静默允许。
- YOLO 模式不会自动执行未被证明安全的 Bash。
- workspace 外路径读写、symlink escape、artifact path 注入都有测试。

## 16.6 TUI、Replay 与命令

- 实时 transcript 与 replay transcript 对同一 `events.jsonl` 输出一致。
- 相邻 assistant delta 合并为一个 assistant block。
- 代码块缩进、空行、连续空格和 CJK 宽字符不被破坏。
- TUI 渲染不因每 token 产生整屏阻塞刷新。
- `replay` 与 `continue` 语义明确；若未实现 continuation，则不存在误导性的 `resume` 命令。

## 16.7 测试与质量门禁

- `cargo fmt --check` 通过。
- `cargo clippy --all-targets --all-features -- -D warnings` 通过。
- `cargo test --all-features` 通过。
- 增加至少以下集成测试组：
  - builtin Tool schema 与 DeepSeek request body。
  - DeepSeek SSE tool call 到真实 Tool 执行。
  - Completion/Finalize 状态机。
  - ToolTurn history projection。
  - SSE byte-safe parser。
  - Retry attempt 事务。
  - Cancellation 与 Bash process group。
  - TUI reducer replay 一致性。

---

# 17. 建议落地顺序与提交边界

建议每个阶段形成独立提交，提交之间保持测试通过：

1. `tool-protocol-v1`：删除 legacy tools，结构化 Tool schema/input/output。
2. `deepseek-tool-calling`：typed DeepSeek request/response 与端到端 ToolCall 测试。
3. `runtime-finalize-state-machine`：Completion、TurnOutcome、统一 finalize。
4. `history-toolturn-projection`：完整 turn 截断与 ToolUse/ToolResult 不变量。
5. `sse-byte-safe-parser`：byte-safe SSE parser 和 attempt retry 语义。
6. `cancellation-process-control`：CancellationToken、TUI 并发、Bash process group。
7. `streaming-event-pipeline`：bounded channel、sequence counter、delta reducer。
8. `provider-capability-cleanup`：删除未接入 capability 或补齐 provider-neutral round-trip。

每个提交必须包含对应回归测试。若某阶段需要临时破坏 API，应优先破坏内部 API，而不是引入兼容层。完成本轮后，再根据真实使用反馈决定是否建立 schema migration；本次重构不以旧 session 可读、旧工具名可用或旧命令行为可用作为目标。

---

# 18. 实施结果与最终验收

## 18.1 重构后的交付状态

本轮按“不需要向后兼容”实施，目标架构已经落地：

1. Tool Protocol v1 只暴露 `Read/Edit/Write/Glob/Grep/ListFiles/Bash`，输入、schema、错误和输出均结构化。
2. Agent 以 completion 和 ToolTurn 不变量驱动循环；成功、失败、取消统一 finalize。
3. `session.json.status` 与 `SessionFinished.outcome` 使用同一组值：`succeeded/failed/cancelled`。
4. DeepSeek 从 HTTP byte stream 解析 SSE；finish reason、tool builder 和重试 attempt 都有明确事务语义。
5. Provider 通过容量为 64 的 bounded channel 推送事件；事件持久化成功后才通知 TUI。
6. CancellationToken 贯穿 TUI、Provider、Approval、ToolContext 和 Bash 进程组。
7. Bash 并发读取 stdout/stderr；内存只保留受限预览，完整截断输出写入 session artifact。
8. TUI 实时与 replay 共用 reducer，相邻 delta 合并，渲染保留代码空白并按 Unicode 终端宽度换行。
9. Runtime 使用真实 System message，并动态附加 workspace、OS、shell 和实际注册工具。
10. 当前定位明确为 DeepSeek/OpenAI-compatible Runtime；未接入的 provider capability 和 `reasoning_effort` 配置已删除。
11. CLI 只保留 `replay <session_id>` 回放语义；没有伪 continuation，也不保留 `resume`。

## 18.2 明确的破坏性变更

- 旧工具名、旧 delimiter 输入和旧字符串 ToolCall 不再支持。
- 旧 session status `completed/interrupted` 不再支持，新值为 `succeeded/cancelled`。
- 旧 `resume` 命令删除，使用 `replay`。
- `reasoning_effort` 配置删除；未知或已移除的配置键会直接报错，不再静默忽略。
- Provider trait 改为 bounded event channel；Tool trait 改为 JSON Value 和结构化 ToolOutput。
- 旧 session schema 不迁移，也不承诺可 replay。

## 18.3 验收证据矩阵

| 验收域 | 实现证据 | 自动化证据 |
|---|---|---|
| Tool 协议 | canonical registry、schema validator、serde input、结构化 ToolError/ToolOutput | registry、7 tools request body、Read/Edit/Write/Bash 单测 |
| Completion/Finalize | TurnCompletion、唯一 finalize、统一终态词汇 | MaxTokens、非法 stop/tool 组合、成功/失败/取消测试 |
| ToolTurn/History | 完整 turn 分组、pending call set、投影前 invariant check | orphan result、tiny budget、多 ToolCall 取消测试 |
| SSE/Retry | byte buffer、SSE frame decoder、request/attempt event | UTF-8/JSON 跨 chunk、CRLF、多行 data、DONE、retry 测试 |
| 取消/安全 | 端到端 token、进程组、离线 allowlist、macOS network sandbox | Provider cancel、Bash timeout/cancel/network/大输出测试 |
| Storage/Event | session 内 O(1) sequence、bounded channel、persist-before-observe | golden JSONL、observer 顺序、唯一终态测试 |
| TUI/Replay | 共享 reducer、33 ms 刷新节流、Unicode width wrap | live/replay、空白、代码缩进、CJK、命令解析测试 |

## 18.4 安全边界

`allow_network=false` 在 macOS 上由离线命令允许列表和 `sandbox-exec` 网络拒绝共同执行。其他平台若没有系统网络 sandbox，则明确退化为严格离线允许列表：解释器、管道、命令替换和未知命令在 Bash 执行入口被拒绝。`flash doctor` 会显示当前平台实际采用的策略。该实现不宣称提供完整文件系统 sandbox。

## 18.5 最终验收标准

本轮完成必须同时满足：

1. 第 16.1-16.6 节全部行为标准都有对应实现和回归测试。
2. 正常、协议错误、Provider 错误和用户取消分别产生唯一且一致的 session 终态。
3. 真实 DeepSeek SSE ToolCall 能执行 canonical `Read` 并将 ToolResult 回灌下一轮。
4. Bash 300 KB 输出测试不死锁，预览被截断且 artifact 保存完整 300 KB。
5. `allow_network=false` 时直接网络命令和解释器间接命令均在执行入口拒绝。
6. 同一事件流的 live/replay transcript 一致，代码空白与 CJK 显示宽度测试通过。
7. 以下命令全部以退出码 0 完成：

```bash
cargo fmt --all -- --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features
```

只有上述标准同时成立，才可将本轮架构重构标记为完成。
