# Spec: flash-code V1

V1 完整设计。本文档覆盖数据层、状态层、能力层、编排层、提示层,是后续所有实现工作的基准——任何代码不符合此处的类型定义或不变量,以本文档为准。

## 章节

- §1-§6 数据层:ContentBlock / Message / History / Compaction / 投影 / 持久化
- §7-§8 Session 接口与全局不变量
- §9 Tool 模型(trait + ApprovalGate + ExecutionContext)
- §10 CompactionPolicy + MicroCompact
- §11 Provider + Capability(OpenAiProvider)
- §12 Agent Loop(半显式状态机)
- §13 System Prompt(静态模板 + 动态环境)

## 0. 设计来源

- ContentBlock 形状参考 Anthropic Messages API,但 Role 拓扑取 OpenAI 4 角色 + 一个 `Summary` 角色(混合方案)
- History 双轨结构(append-only raw + 稀疏 compactions + 实时投影)参考 [opencode `packages/opencode/src/session/compaction.ts`](https://github.com/anomalyco/opencode)
- Reasoning block 投影策略参考 [openai/codex `should_keep_compacted_history_item`](https://github.com/openai/codex)

---

## 1. ContentBlock

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        call_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        call_id: String,
        /// 嵌套 block,但仅允许 Text / Image
        content: Vec<ContentBlock>,
        is_error: bool,
    },
    Image {
        source: ImageSource,
    },
    /// 模型推理过程(Anthropic thinking、OpenAI reasoning)
    Reasoning {
        text: String,
        /// Anthropic thinking 的签名;OpenAI 的 encrypted_content 也走这个字段
        signature: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
}
```

### 1.1 块的角色归属约束

| 块类型 | 允许出现的 Role |
|---|---|
| `Text` | System / User / Assistant / Tool(在 ToolResult 嵌套内)/ Summary |
| `Image` | User / Tool(在 ToolResult 嵌套内) |
| `ToolUse` | **仅 Assistant** |
| `ToolResult` | **仅 Tool** |
| `Reasoning` | **仅 Assistant** |

### 1.2 ToolResult 嵌套限制

`ToolResult.content` 内**仅允许** `Text` 与 `Image`,不允许再嵌套 `ToolUse / ToolResult / Reasoning`。

构造 `ToolResult` 时校验,违反返回 `ContentError::IllegalNestedBlock`。

---

## 2. Message + Role

```rust
pub type MessageId = String;  // uuid v4

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
    /// 压缩摘要专用。Provider 序列化时降级为 user + 前缀。
    /// 用户 / 模型 / 工具产生的消息**不允许**使用此 role。
    Summary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub role: Role,
    pub content: Vec<ContentBlock>,
}
```

### 2.1 构造器(唯一入口)

```rust
impl Message {
    /// 系统提示。content 仅允许 Text。
    pub fn system_text(text: impl Into<String>) -> Self;

    /// 用户消息。content 允许 Text + Image。
    pub fn user(blocks: Vec<ContentBlock>) -> Self;
    pub fn user_text(text: impl Into<String>) -> Self;

    /// 助手消息。content 允许 Text + ToolUse + Reasoning。
    /// 校验:不允许 ToolResult / Image。
    pub fn assistant(blocks: Vec<ContentBlock>) -> Self;

    /// 工具结果消息。content 全部必须是 ToolResult。
    /// 一条 Tool message 可以承载多个 ToolResult(对应同一轮多个 tool_use)。
    pub fn tool_results(results: Vec<ContentBlock>) -> Self;

    /// 压缩摘要。仅由 History::record_compaction 内部使用。
    pub(crate) fn summary(text: String) -> Self;
}
```

构造器执行 §1.1 / §1.2 校验,违反**直接 panic**(内部不变量,不暴露 Result)。

### 2.2 不变量

- `id` 由构造器内部 `uuid::Uuid::new_v4()` 生成,外部无法指定
- 一条 Assistant message 可以同时含 Text + ToolUse + Reasoning;但若含任何 ToolUse,则该 turn 必须紧跟一条 Tool message,且 Tool message 的 ToolResult 集合恰好覆盖所有 ToolUse 的 call_id(由 History 层校验,见 §3.4)

---

## 3. History

```rust
pub struct History {
    messages: Vec<Message>,        // append-only,永不修改 / 删除
    compactions: Vec<Compaction>,  // append-only 稀疏指令流
}
```

### 3.1 字段不变量

- `messages` 严格 append-only:已存在的 Message 不可被删、改、重排
- `compactions` 严格 append-only:同上
- `messages` 中**不允许**出现 `Role::Summary`(Summary 只在投影时生成,不入 raw)

### 3.2 写入接口

```rust
impl History {
    pub fn new() -> Self;

    /// 追加一条 user message。
    pub fn push_user(&mut self, blocks: Vec<ContentBlock>) -> MessageId;

    /// 追加一条 assistant message(含可能的 ToolUse / Reasoning)。
    pub fn push_assistant(&mut self, blocks: Vec<ContentBlock>) -> MessageId;

    /// 追加一条 tool message,内容必须覆盖前一条 assistant message 的所有 ToolUse。
    /// 校验失败返回 Err(HistoryError::ToolResultMismatch { ... })。
    pub fn push_tool_results(
        &mut self,
        results: Vec<ContentBlock>,
    ) -> Result<MessageId, HistoryError>;

    /// 记录一次压缩。校验在 §3.4。
    pub fn record_compaction(
        &mut self,
        c: Compaction,
    ) -> Result<(), HistoryError>;
}
```

### 3.3 读取接口

```rust
impl History {
    /// 全部原始消息(给 UI / 持久化 / 调试用)。
    pub fn raw_messages(&self) -> &[Message];

    /// 最近一次压缩(若有)。
    pub fn last_compaction(&self) -> Option<&Compaction>;

    /// 全部压缩历史(供审计 / replay)。
    pub fn compactions(&self) -> &[Compaction];

    /// 投影:发送给 Provider 的消息列表。每轮调用,实时计算。详见 §5。
    pub fn to_prompt_messages(&self) -> Vec<Message>;

    /// 估算 token 数(用于 CompactionPolicy 决策)。
    pub fn estimate_tokens(&self) -> usize;
}
```

### 3.4 `record_compaction` 硬校验

`Compaction` 入栈前,以下任一条件违反即返回 `Err(HistoryError::*)`,不入栈:

1. **tail_start_id 存在**:`compaction.tail_start_id` 必须能在 `messages` 中找到对应 Message
2. **tail 落在 turn 边界**:`tail_start_id` 指向的 Message 的 `role` 必须是 `User` 或 `Tool`,**不能**是 `Assistant`(避免切在 ToolUse 与对应 ToolResult 之间产生孤儿)
3. **单调递增**:若 `compactions` 非空,新压缩的 `tail_start_id` 在 `messages` 中的位置必须**严格大于**上一次 `compactions.last().tail_start_id` 的位置
4. **非空 tail**:`tail_start_id` 不能是最后一条 message(压缩必须留下至少一条 tail)

错误类型:

```rust
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("tail_start_id {0} not found in history")]
    TailNotFound(MessageId),

    #[error("tail_start_id {0} points to assistant message; must be user or tool")]
    TailNotOnTurnBoundary(MessageId),

    #[error("compaction tail must move forward; new pos {new} <= prev pos {prev}")]
    NonMonotonicCompaction { new: usize, prev: usize },

    #[error("tail cannot be the last message; nothing left to keep")]
    EmptyTail,

    #[error("tool_results call_ids {got:?} do not match preceding assistant tool_uses {expected:?}")]
    ToolResultMismatch { expected: Vec<String>, got: Vec<String> },
}
```

### 3.5 `push_tool_results` 配对校验

调用时:
1. 找到 `messages` 中最后一条 Assistant message
2. 收集其 `ToolUse` 块的 call_id 集合 `expected`
3. 收集传入 `results` 中所有 `ToolResult` 块的 call_id 集合 `got`
4. `expected == got`(集合相等,顺序无关)→ 通过,否则 `ToolResultMismatch`

---

## 4. Compaction

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Compaction {
    /// 自身 uuid
    pub id: String,

    /// 摘要文本(由 Provider 或外部 Policy 产出)
    pub summary: String,

    /// 此 message 及其后保留;之前全部被 summary 替代
    pub tail_start_id: MessageId,

    pub trigger: CompactionTrigger,

    pub created_at: SystemTime,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    /// 阈值触发(token 估算超过 capability.max_context * threshold_ratio)
    Auto,
    /// Provider 报 ContextOverflow,Loop 兜底重压一次
    Overflow,
    /// 用户命令
    Manual,
}
```

`created_at` 仅用于观测/排序展示,**不影响任何逻辑**。Compaction 间的相对顺序由 `compactions` 中的 index 决定。

---

## 5. 投影逻辑(`to_prompt_messages`)

这是整个设计的核心运行时函数。

### 5.1 函数语义

```rust
fn to_prompt_messages(&self) -> Vec<Message> {
    // 无压缩:原样返回,所有 block(含 Reasoning)保留
    let Some(c) = self.compactions.last() else {
        return self.messages.clone();
    };

    // 有压缩:[Summary] + tail 段
    let tail_idx = self.messages.iter()
        .position(|m| m.id == c.tail_start_id)
        .expect("tail_start_id invariant violated by record_compaction");

    let mut out = Vec::with_capacity(self.messages.len() - tail_idx + 1);
    out.push(Message::summary(c.summary.clone()));
    out.extend_from_slice(&self.messages[tail_idx..]);
    out
}
```

### 5.2 Reasoning block 处理

**不需要专门的剥离函数**。投影逻辑天然实现 codex 的策略:

| 区段 | Reasoning 处理 | 原因 |
|---|---|---|
| 压缩点之前 | **不进入投影输出**(被 summary 替代) | 前缀已变,signature/encrypted 必失效 |
| 压缩点之后(tail) | **完整保留** | 前缀未变,签名仍有效,模型需要它接续 reasoning |
| 无压缩 | **完整保留** | 同上 |

### 5.3 Role::Summary 的 Provider 翻译

各 Provider adapter 把 `Role::Summary` 降级:

```rust
// OpenAI adapter
Role::Summary => json!({
    "role": "user",
    "content": format!("<COMPACTION_SUMMARY>\n{}", first_text_block(&msg))
}),
```

降级规则在 adapter 内部实现,Loop / History / Compaction 层完全不知道这件事。

### 5.4 投影性能

- 时间:O(messages.len() - tail_idx + 1),线性切片 + clone
- 空间:O(同上)
- 调用频次:每轮 Loop 入口一次
- 与 LLM 推理时间(秒级)相比可忽略

---

## 6. 持久化(JSONL Sink)

### 6.1 新增/调整事件

```rust
pub enum Event {
    // ... 现有事件保持 ...

    /// 一条 message 入栈。携带完整 Message。
    MessageAppended {
        session_id: String,
        message: Message,
    },

    /// 一次压缩发生。携带完整 Compaction。
    /// 注意:替换字段不再是 replacement: Vec<Message>,而是 tail_start_id。
    HistoryCompacted {
        session_id: String,
        compaction: Compaction,
        before_count: usize,   // raw messages 总数
        tail_count: usize,     // 投影后 tail 段长度
    },
}
```

### 6.2 重建逻辑(Replay)

```rust
fn rebuild_history(events: impl Iterator<Item = Event>) -> History {
    let mut h = History::new();
    for ev in events {
        match ev {
            Event::MessageAppended { message, .. } => {
                h.messages.push(message);  // 内部 unchecked push,因为已经过校验
            }
            Event::HistoryCompacted { compaction, .. } => {
                h.compactions.push(compaction);  // 同上
            }
            _ => {} // 其他事件不影响 history
        }
    }
    h
}
```

**关键性质**:重建过程**零 LLM 调用**,完全确定性,JSONL 体积 = 原始消息体积 + 稀疏压缩点。

### 6.3 与 codex 的差别

codex 的 `RolloutItem::Compacted` 携带 `replacement_history: Option<Vec<ResponseItem>>`(legacy 兼容字段)。我们 v1 不背这个包袱:

- 不存 `replacement`,只存 `tail_start_id`
- Replay 时投影靠 `to_prompt_messages` 实时算

代价是每次发送都要走一次投影,收益是持久化体积更小、不会出现"replacement 与 raw 不一致"的歪斜。

---

## 7. 与上层模块的接口

### 7.1 Session 持有

```rust
pub struct Session {
    pub session_id: String,
    pub system: Vec<Message>,        // 仅 Role::System,跨多轮固定
    pub history: History,            // 替代原来的 Vec<Message>
    pub cancel_root: CancellationToken,
    pub sink: Arc<dyn EventSink>,
}
```

### 7.2 Loop 调用流程

每轮 `run_loop` 入口:

```rust
// 1. 决定是否压缩
if compaction_policy.should_compact(&session.history, &capability) {
    let tail_id = compaction_policy.select_tail(&session.history)?;
    let to_compact = messages_before(&session.history, &tail_id);
    let summary = provider.complete_once(/* summarize prompt */).await?;
    let c = Compaction {
        id: uuid(), summary,
        tail_start_id: tail_id,
        trigger: CompactionTrigger::Auto,
        created_at: SystemTime::now(),
    };
    session.history.record_compaction(c.clone())?;
    sink.emit(Event::HistoryCompacted { ... }).await;
}

// 2. 投影,组装 Prompt
let prompt = Prompt {
    system: session.system.clone(),
    tools: registry.specs(),
    messages: session.history.to_prompt_messages(),
};

// 3. 流式推理...(略)

// 4. 推完后写回 history
session.history.push_assistant(blocks);
sink.emit(Event::MessageAppended { ... }).await;

// 5. 工具执行后
session.history.push_tool_results(results)?;
sink.emit(Event::MessageAppended { ... }).await;
```

### 7.3 Overflow 兜底

Provider 流式返回 `ProviderError::ContextOverflow` 时,Loop 捕获:

```rust
// 重压一次,使用更激进策略
let tail_id = aggressive_policy.select_tail(&session.history)?;
let summary = provider.complete_once(...).await?;
session.history.record_compaction(Compaction {
    trigger: CompactionTrigger::Overflow,
    ...
})?;
// 重试本轮一次。再失败上抛。
```

---

## 8. 不变量总览(Cheat Sheet)

| 不变量 | 守护点 |
|---|---|
| Message.id 唯一 | 构造器内部 uuid v4 |
| ToolUse 仅在 Assistant content | Message::assistant 校验 |
| ToolResult 仅在 Tool message,且其 content 非嵌套违规块 | Message::tool_results + ContentBlock 构造器 |
| Reasoning 仅在 Assistant | Message::assistant 校验 |
| Role::Summary 不入 raw messages | 构造器可见性(`pub(crate)`)+ History 写入接口不暴露 |
| messages append-only | History 接口仅暴露 push_*,无 remove/insert |
| compactions append-only | History 接口仅暴露 record_compaction |
| tail_start_id 存在且落在 turn 边界 | record_compaction §3.4 |
| 压缩点单调递增 | record_compaction §3.4 |
| ToolUse / ToolResult 配对 | push_tool_results §3.5 |
| 投影后 Reasoning 仅在 tail 段保留 | to_prompt_messages 自然行为 §5.2 |
| 持久化重建确定性、零 LLM | Event::MessageAppended + HistoryCompacted §6.2 |

---

## 9. Tool 模型

### 9.0 设计依据

参考对比:

- **codex** 用 `ToolExecutor` 厚 trait(~10 个方法,含 hook / telemetry / diff),`schemars::JsonSchema` 派生 schema,**权限外包**给 `PermissionProfile / ApprovalsReviewer`,输出多态(每工具自己 impl `ToolOutput` trait)。
- **opencode** 用 `tool({description, args, execute})` 三字段工厂,Zod schema,**权限内嵌**(每工具显式 `permission.assert(...)`),输出统一 `ToolResult { output, title, metadata, attachments }`。

我们 v1 的取舍:

- **trait 形态学 opencode**:轻 trait,3 个核心方法。codex 的 hook / telemetry / diff 等扩展点暂不需要,留接口位即可。
- **schema 用 schemars**:Rust 生态事实标准,等价于 zod 的角色。schemars 派生 + serde 反序列化即完成"schema 声明 + 输入校验"两件事。
- **输出统一,学 opencode**:`ToolOutput` 是结构体,不是 trait。所有工具返回同一形状,简化 sink / history / provider 翻译。
- **权限外包,学 codex,但保留显式覆盖位**:框架在 Tool 调用前后统一包一层 `ApprovalGate`,Tool **默认不写权限代码**;特殊工具(如 bash 想做"白名单命令免审批")可以提供 `ToolApprovalAdvice` 影响策略决策,**但不能绕过 gate**。

### 9.1 Tool trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    /// 工具名,等于模型 ToolUse 块里的 name。命名空间扁平,registry 注册时检查重名。
    fn name(&self) -> &str;

    /// 工具自描述,发给模型。
    fn spec(&self) -> ToolSpec;

    /// 风险声明 + 审批建议。框架的 ApprovalGate 综合此值与 ApprovalMode 做决策。
    /// 默认:`ToolApprovalAdvice::default_for(risk_level)`。
    fn approval_advice(&self, _input: &serde_json::Value) -> ToolApprovalAdvice {
        ToolApprovalAdvice::default_for(self.spec().risk)
    }

    /// 执行工具。框架已完成审批 + 输入反序列化校验,实现侧专注业务。
    async fn run(
        &self,
        input: serde_json::Value,
        ctx: &ExecutionContext,
    ) -> ToolOutput;
}
```

只有 4 个方法。`approval_advice` 有默认实现,工具可不实现。

### 9.2 ToolSpec

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,

    /// JSON Schema(由 schemars 从 Input 类型生成)。
    pub input_schema: serde_json::Value,

    /// 静态风险等级,作为 ApprovalGate 的默认输入。
    pub risk: RiskLevel,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// 只读、无副作用(read_file、ls)
    Safe,
    /// 修改本地状态、可恢复(write_file 到工作目录)
    Moderate,
    /// 不可逆 / 影响系统(bash、网络请求)
    Dangerous,
}
```

### 9.3 输入声明:schemars 标准做法

每个工具定义自己的 `Input` 结构体,派生 `Deserialize + JsonSchema`:

```rust
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
struct BashInput {
    /// The shell command to execute via `sh -c`.
    command: String,

    /// Optional timeout in seconds. Defaults to ExecutionContext.timeout.
    #[serde(default)]
    timeout_secs: Option<u64>,
}
```

`spec()` 中通过 `schemars::schema_for!(BashInput)` 生成 schema。框架在调用 `run` 前已经把 `input: Value` 反序列化为 `BashInput`——**反序列化失败即输入校验失败**,统一返回 `ToolOutput::failure_invalid_input(error)`,不进入 `run`。

### 9.4 ToolOutput(统一结构,非 trait)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    /// 给模型看的内容。复用 Message 的 ContentBlock,但仅允许 Text / Image。
    pub content: Vec<ContentBlock>,

    /// 是否失败。失败时 content 通常含错误描述文本。
    pub is_error: bool,

    /// 给 UI / 日志展示的简短标题(可选)。
    pub title: Option<String>,

    /// 结构化元数据(给 UI 渲染,不进 prompt)。
    pub metadata: serde_json::Value,
}

impl ToolOutput {
    /// 普通文本成功。
    pub fn text(s: impl Into<String>) -> Self;
    /// 富 block 成功。
    pub fn blocks(blocks: Vec<ContentBlock>) -> Self;
    /// 业务失败。content 为错误描述,is_error = true。
    pub fn failure(msg: impl Into<String>) -> Self;
    /// 输入反序列化失败。框架内部使用,业务不调。
    pub(crate) fn failure_invalid_input(err: serde_json::Error) -> Self;
    /// 取消。框架在 cancel 路径生成,业务不调。
    pub(crate) fn cancelled() -> Self;
}
```

**注意**:`ToolOutput` 中允许的 ContentBlock 与 §1.1 保持一致——只能是 `Text` 或 `Image`,不能是嵌套 `ToolUse / ToolResult / Reasoning`。构造器校验。

### 9.5 ToolOutput → Message 的转换

框架在 `run` 返回后,把 `ToolOutput` 包成 `ContentBlock::ToolResult`,然后 `History::push_tool_results` 入栈:

```rust
fn output_to_block(call_id: &str, out: &ToolOutput) -> ContentBlock {
    ContentBlock::ToolResult {
        call_id: call_id.to_owned(),
        content: out.content.clone(),
        is_error: out.is_error,
    }
}
```

`title / metadata` **不进 history**(模型看不到),只走 Event 流给 sink/UI。

### 9.6 ExecutionContext

```rust
pub struct ExecutionContext {
    pub session_id: String,
    pub call_id: String,

    /// 工作目录。Bash 等工具应在此 cwd 下执行。
    pub cwd: PathBuf,

    /// 超时上限。框架在 `run` 外层包 `tokio::time::timeout`,超时返回 ToolOutput::failure。
    pub timeout: Duration,

    /// 单次工具输出最大字节数(stdout + stderr 合计)。框架在 BashTool 等工具内消费。
    pub max_output_bytes: usize,

    /// 取消 token。Tool 内部 select! 监听。
    pub cancel: CancellationToken,

    /// Sink 句柄,允许 Tool 主动 emit 进度事件(如 bash 流式 stdout)。
    pub sink: Arc<dyn EventSink>,
}
```

**框架兜底**:在调用 `tool.run(input, ctx)` 时,框架外层包 `timeout(ctx.timeout, ...)` + `select! { _ = ctx.cancel.cancelled() => ... }`,**Tool 即使忘记处理超时和取消也不会失控**——超时返回 `ToolOutput::failure("timeout after Ns")`,取消返回 `ToolOutput::cancelled()`。

`max_output_bytes` 由具体 Tool(如 Bash)在读 stdout 时执行。框架不强制,但 ExecutionContext 提供统一的值,工具不要自己写魔法数字。

### 9.7 ApprovalGate(权限的统一外包)

权限不是 Tool 的事,是框架的事。

```rust
pub struct ApprovalGate {
    pub mode: ApprovalMode,
}

#[derive(Debug, Clone)]
pub struct ToolApprovalAdvice {
    /// 工具基于本次具体输入给出的建议。
    pub decision: ApprovalDecision,
    /// 建议的简短原因(用于日志 / UI)。
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// 工具明确说"这次调用安全,可自动放行"(覆盖默认风险)。
    AutoApprove,
    /// 工具明确说"这次必须问用户"(覆盖 Yolo 也不能放行)。
    MustAsk,
    /// 工具不发表意见,听 ApprovalGate 默认决策。
    Default,
}

impl ToolApprovalAdvice {
    pub fn default_for(risk: RiskLevel) -> Self {
        Self { decision: ApprovalDecision::Default, reason: None }
    }
}
```

**Gate 决策表**(给定 `ApprovalMode + RiskLevel + ToolApprovalAdvice` → `Outcome`):

| Mode | Risk | Advice | Outcome |
|---|---|---|---|
| Yolo | 任意 | MustAsk | **Ask user** |
| Yolo | 任意 | 其它 | AutoApprove |
| Default | Safe | Default | AutoApprove |
| Default | 任意 | AutoApprove | AutoApprove |
| Default | Moderate / Dangerous | Default / MustAsk | Ask user |

**关键约束**:`MustAsk` 即使在 Yolo 模式下也强制问。这是工具自我保护机制(如 bash 检测到 `rm -rf /` 想强制问)。

`AutoApprove` 不能覆盖"模型生成的高危调用",仅当工具实现者**明确判断当前输入安全**时使用(如 read_file 收到任何路径都安全)。

### 9.8 ToolRegistry

```rust
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self;

    /// 注册工具。重名 panic(启动期错误,不该静默)。
    pub fn register(&mut self, tool: Arc<dyn Tool>);

    /// 查找。未知工具返回 None,Loop 据此发出 ToolError 而非 panic。
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>>;

    /// 全部 spec(给 Provider 组装 Prompt.tools)。
    pub fn specs(&self) -> Vec<ToolSpec>;
}
```

**重要**:`get` 返回 `Option`,**Loop 不再 panic**(修上一轮审计 #4)。未知工具的处理:

```rust
match registry.get(&call.name) {
    Some(tool) => { /* 正常执行 */ }
    None => {
        // 模型编了不存在的工具名:发 ToolError,回填 tool_result(is_error=true),让模型自纠
        sink.emit(Event::ToolError {
            call_id: call.call_id.clone(),
            error: format!("unknown tool: {}", call.name),
            ..
        }).await;
        ToolOutput::failure(format!("unknown tool: {}", call.name))
    }
}
```

### 9.9 完整调用链(框架统一包装)

```rust
async fn invoke(
    tool: &dyn Tool,
    call: &ToolCall,
    gate: &ApprovalGate,
    ctx: &ExecutionContext,
    approval_io: &mut dyn ApprovalIO,
) -> ToolOutput {
    // 1. 输入校验(Tool 自己定义 Input,框架反序列化)
    let advice = tool.approval_advice(&call.input);

    // 2. ApprovalGate 决策
    match gate.decide(tool.spec().risk, advice) {
        Outcome::AutoApprove => {}
        Outcome::Ask => {
            if !approval_io.ask(call).await {
                return ToolOutput::failure("user rejected this tool call");
            }
        }
    }

    // 3. 框架包 timeout + cancel
    let fut = tool.run(call.input.clone(), ctx);
    tokio::select! {
        out = tokio::time::timeout(ctx.timeout, fut) => match out {
            Ok(out) => out,
            Err(_) => ToolOutput::failure(format!("timeout after {:?}", ctx.timeout)),
        },
        _ = ctx.cancel.cancelled() => ToolOutput::cancelled(),
    }
}
```

**Tool 实现者的契约**:
- 不写 timeout / cancel 兜底(框架包了)
- 不写权限检查(ApprovalGate 包了)
- 不自己 emit `ToolStart / ToolEnd / ToolCancelled`(框架包了)
- **可以**主动 emit 进度事件(如 bash 流式 stdout 通过 `ctx.sink`)

### 9.10 BashTool 范例

```rust
pub struct BashTool;

#[derive(Deserialize, JsonSchema)]
struct BashInput {
    /// The shell command to execute.
    command: String,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str { "bash" }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "Execute a shell command via `sh -c`. Output truncated to ctx.max_output_bytes.".into(),
            input_schema: serde_json::to_value(schemars::schema_for!(BashInput)).unwrap(),
            risk: RiskLevel::Dangerous,
        }
    }

    fn approval_advice(&self, input: &serde_json::Value) -> ToolApprovalAdvice {
        // 例:检测到 rm -rf 强制问,即使 Yolo 模式
        let cmd = input["command"].as_str().unwrap_or("");
        if cmd.contains("rm -rf") {
            return ToolApprovalAdvice {
                decision: ApprovalDecision::MustAsk,
                reason: Some("destructive: rm -rf detected".into()),
            };
        }
        ToolApprovalAdvice::default_for(RiskLevel::Dangerous)
    }

    async fn run(&self, input: serde_json::Value, ctx: &ExecutionContext) -> ToolOutput {
        let parsed: BashInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolOutput::failure(format!("invalid input: {e}")),
        };

        // 业务核心:spawn + 同时读 stdout/stderr(避免 pipe 满死锁)
        let mut child = match Command::new("sh").arg("-c").arg(&parsed.command)
            .current_dir(&ctx.cwd)
            .stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return ToolOutput::failure(format!("spawn failed: {e}")),
        };

        // 读到 max_output_bytes 截断;wait_with_output 同时消费两个 pipe,不会死锁
        let out = match child.wait_with_output().await {
            Ok(o) => o,
            Err(e) => return ToolOutput::failure(e.to_string()),
        };

        let stdout = truncate(String::from_utf8_lossy(&out.stdout).into(), ctx.max_output_bytes);
        let stderr = truncate(String::from_utf8_lossy(&out.stderr).into(), ctx.max_output_bytes);

        if out.status.success() {
            ToolOutput::text(stdout)
        } else {
            ToolOutput::failure(format!(
                "exit {}: {}", out.status.code().unwrap_or(-1), stderr
            ))
        }
    }
}
```

注意这一版相比当前 `src/tool/bash.rs`:
- 没有 `tokio::select! { wait, cancelled }`——框架包了
- 没有 take stdout/stderr 之后再 read 的死锁路径——`wait_with_output` 一次搞定(修上一轮审计 #3)
- `cwd` 来自 ctx,不再是进程 cwd
- `rm -rf` 在 Yolo 模式下也强制问(`MustAsk`)

### 9.11 Tool 模型不变量(补充到 §8 Cheat Sheet)

| 不变量 | 守护点 |
|---|---|
| Tool 名唯一 | `ToolRegistry::register` panic |
| Tool 输入反序列化失败 → 不进 run | 框架在 invoke 第 1 步校验 |
| 超时 / 取消由框架兜底,Tool 不必处理 | `invoke` §9.9 外层 select |
| ToolOutput.content 仅含 Text / Image | `ToolOutput` 构造器校验 |
| `MustAsk` 即使 Yolo 也问 | `ApprovalGate::decide` §9.7 |
| 未知工具不 panic | `ToolRegistry::get` 返回 Option,Loop 兜底 |

### 9.12 v1 不做(明确红线)

- **MCP 协议适配**:Tool 接入 MCP server 留到 v2
- **流式工具结果**:`run` 一次返回,中间进度通过 `ctx.sink` 主动 emit,但不改变最终 ToolOutput 是单次返回这个事实
- **Tool 内嵌权限调用**:不允许 Tool 自己 `permission.assert(...)`——必须走 `ToolApprovalAdvice` 影响 ApprovalGate 决策。统一管控点只有一个。
- **跨 Tool 的资源共享**(数据库连接池、HTTP client cache)

---

## 10. CompactionPolicy + MicroCompact

### 10.0 设计依据

参考对比(三家):

| 维度 | codex | opencode | Claude Code |
|---|---|---|---|
| 选 tail 策略 | 反向取 user msg + 20k token 上限 | `tail_turns` × `preserve_recent_tokens` 双约束 | 多模板 + partial compact 双向 |
| token 估算 | tokenizer | 启发式 `JSON.stringify.len/4` | 预测式(含 `maxTurnGrowth`) |
| Overflow 兜底 | 循环剥旧 + 重摘 | 单次 reactive,失败终止 | 三阶段(collapse / reactive / 摘要重试 ×3) |
| 摘要 prompt | 单模板 | 显式 `<previous-summary>` | 三模板(base / partial-from / partial-up-to) |
| Policy 抽象 | 函数化 | Service 类 + plugin hooks | 多 Service + pre/post hooks |
| MicroCompact | 无 | 简化版 `prune` | **专门一层** |

V1 的取舍:

- **trait + 默认实现**:可替换,但 v1 只提供 `DefaultPolicy`
- **select_tail 学 opencode**:turn × token 双约束,token 优先
- **token 启发式 + reserved buffer 学 opencode**:不引入 tiktoken 依赖
- **Overflow 单次兜底学 opencode**:更激进参数重压一次,失败上抛
- **预测式触发学 Claude Code**:`maxTurnGrowth` 提前一轮压缩,避免溢出体感
- **MicroCompact 学 Claude Code**:append-only 不变量保住,只在投影时 redact;turn 距离 + 数量 + 大小三重过滤

### 10.1 CompactionPolicy trait

```rust
#[async_trait]
pub trait CompactionPolicy: Send + Sync {
    /// 是否应该触发压缩。Loop 在每轮入口调用。
    fn should_compact(&self, history: &History, capability: &Capability) -> bool;

    /// 选择 tail 边界。返回 `tail_start_id`(必须满足 §3.4 的约束)。
    /// 返回 None 表示找不到合适的边界(比如 history 太短),Loop 据此跳过本次压缩。
    fn select_tail(&self, history: &History) -> Option<MessageId>;

    /// Overflow 兜底时的更激进 tail 选择。
    /// 默认实现复用 `select_tail` 但允许子类型用更小的 keep_last_turns。
    fn select_tail_overflow(&self, history: &History) -> Option<MessageId>;

    /// 摘要 prompt 的 system 段。详见 §10.4。
    fn summary_prompt(&self) -> &str;
}
```

只有 4 个方法,三个数据相关 + 一个 prompt。Async 仅留接口位(实现可同步),供未来需要查询外部状态的策略扩展。

### 10.2 DefaultPolicy

```rust
pub struct DefaultPolicy {
    /// 最少保留的 turn 数(turn = 一组 user → assistant → [tool] 序列)
    pub keep_last_turns: usize,           // 默认 2

    /// tail 段最大 token 数。token 上限优先于 turn 数:
    /// 若按 turn 选出的段超过此值,则缩减 turn 数直到塞下。
    pub max_tail_tokens: usize,           // 默认 20_000

    /// 触发阈值预留:估算 tokens + maxTurnGrowth > capability.max_context - reserved_tokens 时压缩
    pub reserved_tokens: usize,           // 默认 20_000

    /// Overflow 兜底时的更激进 turn 上限
    pub overflow_keep_last_turns: usize,  // 默认 1
}

impl Default for DefaultPolicy {
    fn default() -> Self {
        Self {
            keep_last_turns: 2,
            max_tail_tokens: 20_000,
            reserved_tokens: 20_000,
            overflow_keep_last_turns: 1,
        }
    }
}
```

四个字段语义:
- `keep_last_turns`:tail 至少包含这么多个 turn(下限)
- `max_tail_tokens`:tail token 不能超过这个数(上限,可能反向缩减 turn 数)
- `reserved_tokens`:预留给本轮 + 工具结果的 token 余量(buffer)
- `overflow_keep_last_turns`:Overflow 兜底专用,通常远小于 `keep_last_turns`

### 10.3 should_compact 实现(预测式)

```rust
impl CompactionPolicy for DefaultPolicy {
    fn should_compact(&self, history: &History, capability: &Capability) -> bool {
        let current = history.estimate_tokens();
        let next_turn_growth = estimate_last_turn_tokens(history); // 取最近 1 turn 作下一轮估算

        let usable = capability.max_context.saturating_sub(self.reserved_tokens);
        current + next_turn_growth > usable
    }
    // ...
}

/// 估算最近 1 个 turn 的 token 数,作为下一轮增长的保守预测。
/// 若 history 不足 1 个 turn,返回固定保底值 2_000。
fn estimate_last_turn_tokens(history: &History) -> usize {
    let n = history.raw_messages().len();
    if n == 0 { return 2_000; }
    let start = n.saturating_sub(3); // 一个 turn ≈ user + assistant + tool 三条
    history.raw_messages()[start..].iter()
        .map(|m| estimate_message_tokens(m))
        .sum()
}
```

**预测式触发的意义**:在还没真正溢出时就压一次,避免"刚好触发 Overflow → reactive 兜底"的卡顿(这是 Claude Code 的洞察)。`maxTurnGrowth` 简单实现:取最近 1 个 turn 的 token 数,假设下一轮规模相近。

`History::estimate_tokens` 实现(对应之前 §3.3 的接口):

```rust
fn estimate_tokens(&self) -> usize {
    self.to_prompt_messages().iter()
        .map(estimate_message_tokens)
        .sum()
}

fn estimate_message_tokens(msg: &Message) -> usize {
    // 启发式:JSON 序列化后字符数 / 4。简单、足够指导压缩决策。
    serde_json::to_string(msg).map(|s| s.len() / 4).unwrap_or(0)
}
```

注意 `estimate_tokens` 是基于**投影后**的 messages 算的——这样 MicroCompact 已经 redact 的部分不会被算进阈值,触发更精准。

### 10.4 select_tail 实现(turn × token 双约束)

```rust
impl CompactionPolicy for DefaultPolicy {
    fn select_tail(&self, history: &History) -> Option<MessageId> {
        select_tail_with_limit(history, self.keep_last_turns, self.max_tail_tokens)
    }

    fn select_tail_overflow(&self, history: &History) -> Option<MessageId> {
        // Overflow 兜底:更激进,turn 数减半,token 上限也减半
        select_tail_with_limit(history, self.overflow_keep_last_turns, self.max_tail_tokens / 2)
    }
}

/// 选 tail 的核心算法:turn × token 双约束,token 优先。
fn select_tail_with_limit(
    history: &History,
    target_turns: usize,
    max_tokens: usize,
) -> Option<MessageId> {
    let messages = history.raw_messages();
    if messages.len() < 3 { return None; } // 至少 3 条才有压的必要

    // 第一步:从尾部反向数 target_turns 个 turn 边界
    // turn 边界 = 一条 User message。从最近的开始数。
    let user_indices: Vec<usize> = messages.iter().enumerate()
        .filter(|(_, m)| m.role == Role::User)
        .map(|(i, _)| i)
        .collect();

    if user_indices.len() <= target_turns {
        return None; // 不够 target_turns + 1 个 turn,没必要压
    }

    // tail 候选起点 = 倒数第 target_turns 个 user message
    let mut candidate_idx = user_indices[user_indices.len() - target_turns];

    // 第二步:token 上限收紧。从 candidate_idx 开始累计 token,超限则向后推进 candidate
    loop {
        let tail_tokens: usize = messages[candidate_idx..].iter()
            .map(estimate_message_tokens)
            .sum();
        if tail_tokens <= max_tokens { break; }

        // 推进到下一个 user message(下一个 turn 边界)
        match user_indices.iter().find(|&&i| i > candidate_idx) {
            Some(&next) => candidate_idx = next,
            None => return None, // 退到最末仍超限,放弃
        }
    }

    // 校验:tail 不能是最后一条 message(§3.4 约束 4)
    if candidate_idx >= messages.len() - 1 {
        return None;
    }

    Some(messages[candidate_idx].id.clone())
}
```

算法语义:
1. 反向取 `target_turns` 个 user message 作为 tail 起点候选(turn 边界天然对齐 §3.4 约束 2)
2. 若 tail 段 token 超限,**逐个 turn 向后推进**直到塞下
3. 若推到末尾仍超限或 tail 为空,返回 None,Loop 跳过本次压缩

**为什么 token 优先**:turn 数是用户体感保护(至少看到最近几轮原文),token 上限是硬约束(不能给摘要请求自己留出空间)。冲突时硬约束胜出。

### 10.5 摘要 prompt(显式 previous-summary 衔接)

```rust
const DEFAULT_SUMMARY_PROMPT: &str = r#"
You are a conversation summarization assistant. Your job is to compress the older
portion of an ongoing conversation between a user and a coding agent into a concise
summary, while preserving everything needed for the agent to continue the work.

Input format:
- A series of messages tagged with role (user / assistant / tool).
- May begin with a <previous-summary> block: this is a summary from an earlier
  compaction. Treat it as already-condensed context to merge into your new summary,
  not as new content to summarize again.

Output a single paragraph (or short bulleted list) that captures:
1. User's explicit goals and constraints
2. Key decisions made and rationale
3. Files / commands / data referenced
4. Errors encountered and how they were resolved
5. Outstanding TODOs or open questions

Be factual and dense. Do not editorialize. Do not add information not present in
the input. Do not include role tags or formatting from the input.
"#;
```

调用时把待压缩段(`messages[..tail_idx]`)序列化成纯文本喂给 provider:

```rust
fn serialize_for_compaction(messages: &[Message]) -> String {
    messages.iter().map(|m| {
        let role = format!("{:?}", m.role).to_lowercase();
        let content = render_blocks_as_text(&m.content);
        // 若投影到的第一条已经是 Summary,标记为 <previous-summary>
        if m.role == Role::Summary {
            format!("<previous-summary>\n{}\n</previous-summary>", content)
        } else {
            format!("[{}] {}", role, content)
        }
    }).collect::<Vec<_>>().join("\n\n")
}
```

**注意**:第二次及之后的压缩,待压缩段的**第一条 message 实际上是上一次压缩生成的 summary**(因为投影时它在最前面)——`serialize_for_compaction` 据此包成 `<previous-summary>` 块,prompt 会让模型把它当已压缩上下文合并而非重复总结。这是 opencode 同款做法。

### 10.6 完整压缩流程(Loop 视角)

```rust
async fn maybe_compact(
    history: &mut History,
    policy: &dyn CompactionPolicy,
    provider: &dyn Provider,
    capability: &Capability,
    sink: Arc<dyn EventSink>,
    session_id: &str,
) -> Result<bool, Error> {
    if !policy.should_compact(history, capability) {
        return Ok(false);
    }
    let Some(tail_id) = policy.select_tail(history) else {
        return Ok(false);
    };

    let summary = run_summary(history, &tail_id, policy, provider).await?;

    let c = Compaction {
        id: uuid::Uuid::new_v4().to_string(),
        summary,
        tail_start_id: tail_id,
        trigger: CompactionTrigger::Auto,
        created_at: SystemTime::now(),
    };
    let before = history.raw_messages().len();
    history.record_compaction(c.clone())?;
    let tail_count = history.to_prompt_messages().len() - 1; // 减去 Summary 占位

    sink.emit(Event::HistoryCompacted {
        session_id: session_id.to_owned(),
        compaction: c,
        before_count: before,
        tail_count,
    }).await;

    Ok(true)
}

async fn run_summary(
    history: &History,
    tail_id: &MessageId,
    policy: &dyn CompactionPolicy,
    provider: &dyn Provider,
) -> Result<String, Error> {
    let to_compact = history.messages_before(tail_id);
    let body = serialize_for_compaction(&to_compact);
    provider.complete_once(&Prompt {
        system: vec![Message::system_text(policy.summary_prompt())],
        tools: vec![],
        messages: vec![Message::user_text(body)],
    }).await
}
```

调用顺序约定:**先 micro,再 compact**(§10.8 详述)。`maybe_compact` 在 Loop 入口被调用一次,触发后才走 `run_summary` 实际跑摘要请求。

### 10.7 Overflow 兜底流程

```rust
async fn overflow_compact(
    history: &mut History,
    policy: &dyn CompactionPolicy,
    provider: &dyn Provider,
    sink: Arc<dyn EventSink>,
    session_id: &str,
) -> Result<(), Error> {
    let Some(tail_id) = policy.select_tail_overflow(history) else {
        return Err(Error::ContextOverflow); // history 太短,无法兜底
    };

    let summary = run_summary(history, &tail_id, policy, provider).await
        .map_err(|_| Error::ContextOverflow)?;

    let c = Compaction {
        id: uuid::Uuid::new_v4().to_string(),
        summary,
        tail_start_id: tail_id,
        trigger: CompactionTrigger::Overflow,
        created_at: SystemTime::now(),
    };
    let before = history.raw_messages().len();
    history.record_compaction(c.clone())?;
    let tail_count = history.to_prompt_messages().len() - 1;

    sink.emit(Event::HistoryCompacted {
        session_id: session_id.to_owned(),
        compaction: c,
        before_count: before,
        tail_count,
    }).await;
    Ok(())
}
```

Loop 集成:provider stream 报 `ContextOverflow` 时,Loop 捕获 → 调一次 `overflow_compact` → 重试当前轮一次。**第二次仍 Overflow 直接上抛**,不再循环兜底(opencode 同款,避免无限重试)。

```rust
// Loop 内伪代码
let mut overflow_attempted = false;
loop {
    match stream_model(...).await {
        Err(Error::ContextOverflow) if !overflow_attempted => {
            overflow_attempted = true;
            overflow_compact(...).await?;
            continue;
        }
        Err(e) => return Err(e),
        Ok(result) => break result,
    }
}
```

### 10.8 MicroCompact

#### 10.8.1 设计原则

学 Claude Code,严格遵守:

1. **Append-only 不变量保留**:`messages` 永不修改
2. **不持久化 redaction 状态**:每次投影按规则现算,天然幂等
3. **投影时应用**:`to_prompt_messages` 走完压缩切片后,再叠加一层 micro redact
4. **元信息走事件,占位符干净**:模型看到的占位符是简短常量字符串,触发的元信息(call_id 列表 / 节省的 token 数)只通过 `Event::MicroCompacted` 给 sink/UI

#### 10.8.2 MicroCompactPolicy

```rust
pub struct MicroCompactPolicy {
    /// 距今 turn 数超过此值的 tool_result 才会被 redact
    pub stale_after_turns: usize,        // 默认 4

    /// 永远保留最近 N 个 tool_result(无论是否超龄、是否超大)
    pub keep_recent: usize,              // 默认 5

    /// 单条 tool_result 内容超过此字节数才会被 redact(避免压短输出)
    pub size_threshold_bytes: usize,     // 默认 4_000
}

impl Default for MicroCompactPolicy {
    fn default() -> Self {
        Self {
            stale_after_turns: 4,
            keep_recent: 5,
            size_threshold_bytes: 4_000,
        }
    }
}

const MICRO_PLACEHOLDER: &str = "[Old tool result content cleared]";
```

**redact 规则**(三个条件**全满足**才 redact):

```
turn_distance(msg) >= stale_after_turns
    AND total_size(tool_result) >= size_threshold_bytes
    AND NOT in_keep_recent_window(msg, keep_recent)
```

**为什么三个一起**:
- 仅 turn 距离:会把"刚跑的 1 行 echo"也清掉,无意义
- 仅 size:会把"刚读的 5MB 文件"立刻清掉,模型本轮还没用上
- 仅 keep_recent:大文件挤掉重要小输出

#### 10.8.3 apply_micro_compact 实现

```rust
/// 在投影出的 messages 上原地 redact。返回 (被 redact 的 call_id 列表, 节省的字节数)。
fn apply_micro_compact(
    messages: &mut [Message],
    policy: &MicroCompactPolicy,
) -> MicroCompactResult {
    // 第一步:收集所有 ToolResult 块的 (msg_index, block_index, call_id, size)
    let mut tool_results: Vec<ToolResultRef> = Vec::new();
    for (mi, msg) in messages.iter().enumerate() {
        if msg.role != Role::Tool { continue; }
        for (bi, block) in msg.content.iter().enumerate() {
            if let ContentBlock::ToolResult { call_id, content, .. } = block {
                let size = content_size_bytes(content);
                tool_results.push(ToolResultRef {
                    msg_idx: mi, block_idx: bi,
                    call_id: call_id.clone(),
                    size,
                });
            }
        }
    }

    if tool_results.is_empty() {
        return MicroCompactResult::default();
    }

    let total = tool_results.len();

    // 第二步:对每个 tool_result 判断是否 redact
    let mut redacted_ids = Vec::new();
    let mut bytes_saved = 0usize;

    for (rank, tr) in tool_results.iter().enumerate() {
        // rank: 0 是最旧,total-1 是最新。距今 turn 距离 = total - 1 - rank
        let distance = total - 1 - rank;
        let in_keep_recent = distance < policy.keep_recent;

        if in_keep_recent { continue; }
        if distance < policy.stale_after_turns { continue; }
        if tr.size < policy.size_threshold_bytes { continue; }

        // 命中三条件,执行 redact
        if let ContentBlock::ToolResult { content, .. } =
            &mut messages[tr.msg_idx].content[tr.block_idx]
        {
            *content = vec![ContentBlock::Text { text: MICRO_PLACEHOLDER.to_owned() }];
        }
        redacted_ids.push(tr.call_id.clone());
        bytes_saved += tr.size;
    }

    MicroCompactResult { redacted_ids, bytes_saved }
}

struct ToolResultRef {
    msg_idx: usize,
    block_idx: usize,
    call_id: String,
    size: usize,
}

#[derive(Default)]
pub struct MicroCompactResult {
    pub redacted_ids: Vec<String>,
    pub bytes_saved: usize,
}

fn content_size_bytes(blocks: &[ContentBlock]) -> usize {
    blocks.iter().map(|b| match b {
        ContentBlock::Text { text } => text.len(),
        ContentBlock::Image { source } => match source {
            ImageSource::Base64 { data, .. } => data.len(),
            ImageSource::Url { url } => url.len(),
        },
        _ => 0,  // ToolResult 嵌套规则禁止其他 block,这里防御性归零
    }).sum()
}
```

**关键性质**:
- 函数对 `messages` 切片就地操作,**不动 History 内部状态**
- 幂等:多次调用结果相同(redacted 后的 ToolResult 已经只剩占位符 Text,size < threshold,下次自动跳过)
- 决定性:仅依赖输入 + policy,不依赖墙钟、不依赖随机数

#### 10.8.4 投影函数升级

```rust
impl History {
    pub fn to_prompt_messages(&self) -> Vec<Message> {
        let mut out = self.compact_slice();
        apply_micro_compact(&mut out, &self.micro_policy);
        out
    }

    /// 仅做压缩切片,不应用 micro。供测试 / 调试使用。
    fn compact_slice(&self) -> Vec<Message> {
        match self.compactions.last() {
            None => self.messages.clone(),
            Some(c) => {
                let idx = self.messages.iter()
                    .position(|m| m.id == c.tail_start_id)
                    .expect("tail_start_id invariant");
                let mut v = Vec::with_capacity(self.messages.len() - idx + 1);
                v.push(Message::summary(c.summary.clone()));
                v.extend_from_slice(&self.messages[idx..]);
                v
            }
        }
    }
}
```

`History` 结构对应增加字段:

```rust
pub struct History {
    messages: Vec<Message>,
    compactions: Vec<Compaction>,
    micro_policy: MicroCompactPolicy,   // 新增
}
```

#### 10.8.5 调用顺序约定

**每轮 Loop 入口顺序固定**:

```
1. micro 通过投影自动应用(包含在 to_prompt_messages 内)
2. should_compact 判定(基于已 micro 的 token 估算)
3. 若需要,select_tail + run_summary + record_compaction
4. 用最终的 to_prompt_messages 组装 Prompt 发送
```

**为什么 micro 在 compact 之前**:
- micro 减少了 token 总量,可能让 should_compact 不再触发(节省一次摘要请求)
- 即使 compact 仍触发,摘要器看到的也是 micro 后的版本——摘要本身省 token
- 这与 Claude Code 的调用顺序一致

#### 10.8.6 事件

```rust
pub enum Event {
    // ... 现有事件 ...

    MicroCompacted {
        session_id: String,
        redacted_ids: Vec<String>,   // 被清的 tool_call_id 列表
        bytes_saved: usize,
    },
}
```

**触发时机**:Loop 在调用 `to_prompt_messages` 后,若返回的 `MicroCompactResult.redacted_ids` 非空,emit 一次。

**注意**:`MicroCompacted` 事件**不影响 replay**——重建 history 不需要它,因为 redact 是投影时计算的,raw messages 完全不变。它只是给 sink/UI 的可观测信号。

### 10.9 v1 不做(明确红线)

- **partial compact**(从 pivot 之后压 / 之前压):Claude Code 独有,v1 不需要
- **context collapse**:Claude Code 自己的实现也是 stub,跳过
- **三阶段 overflow 重试**:v1 单次兜底失败即上抛
- **multi-pass 摘要**:待压缩段超长时不分段,直接走 overflow 兜底路径
- **MicroCompact 对 Image / Reasoning 块**:Claude Code 做了,v1 先不做(避免动 ContentBlock 投影逻辑;若需要可加在 `apply_micro_compact` 的 collect 阶段)
- **基于墙钟的触发**:用 turn 距离替代,确定性 / replay 友好
- **plugin hooks**(改 prompt / autocontinue):v1 没插件系统
- **真 tokenizer**:启发式 + reserved buffer 已经够用

### 10.10 不变量补充(并入 §8 Cheat Sheet)

| 不变量 | 守护点 |
|---|---|
| Compaction 不修改 raw messages | `record_compaction` 仅 push 进 compactions |
| MicroCompact 不修改 raw messages | `apply_micro_compact` 仅作用于投影后的 Vec |
| `to_prompt_messages` 幂等(对同一 history 多次调用结果相同) | 投影函数纯函数,无内部状态 |
| MicroCompact 后再次调用仍幂等 | 占位符 size < threshold,自动跳过 |
| Overflow 兜底单次,失败即上抛 | Loop 用 `overflow_attempted: bool` 限制 |
| 摘要请求自身的 messages 不带 tools | `run_summary` 构造 `Prompt { tools: vec![], ... }` |
| 预测式触发避免 just-in-time overflow | `should_compact` 加 `maxTurnGrowth` |

---

## 11. Provider + Capability

### 11.0 设计依据与 V1 红线

V1 现实:
- **只有一个 provider 实现**:OpenAI Chat Completions 兼容(覆盖 OpenAI / Azure / OpenRouter / 国产兼容服务)
- **Tool use 是硬要求**:不支持 tool use 的模型不在目标范围,任何 Provider 实现必须满足
- **流式是硬要求**:同上
- **Reasoning 支持**:保留(支持 o-series / 兼容服务的 reasoning)
- **不传图片**:V1 完全不发 `ContentBlock::Image`,Provider 也不需要处理

`Provider` trait 存在的**唯一目的**:测试可替换(`MockProvider` 给 Loop / Compaction 测试用)。**不为将来支持 Anthropic / Bedrock / Gemini 做抽象**——真要支持时再回头改 trait,代价比现在过度抽象低得多。

参考的对比信息(codex / opencode / Claude Code 做了多 provider 抽象)V1 不照搬,记录于本节最末备查。

### 11.1 Capability

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    /// 模型上下文窗口总 token 数(输入 + 输出 + system + tools 全部计入)
    pub max_context: usize,

    /// 模型单次响应最大输出 token 数
    pub max_output: usize,

    /// 是否支持 reasoning(对应 ContentBlock::Reasoning;o-series / 兼容服务的 reasoning)
    /// false 时:adapter 在请求装配阶段剥离 history 中的 Reasoning block
    pub supports_reasoning: bool,
}

impl Capability {
    pub fn openai_gpt4o() -> Self {
        Self { max_context: 128_000, max_output: 16_384, supports_reasoning: false }
    }

    pub fn openai_o_series(max_context: usize, max_output: usize) -> Self {
        Self { max_context, max_output, supports_reasoning: true }
    }
}
```

**为什么字段这么少**:
- 删 `supports_tools`:tool use 是硬要求(§11.0),没必要在数据里再开关
- 删 `supports_streaming`:同上
- 删 `supports_image_input`:V1 不传图,无意义
- 留 `supports_reasoning`:同样支持 OpenAI 兼容的非 o-series 模型,需要根据这个字段决定剥不剥 Reasoning block

Capability 是**只读数据**,Provider 构造时确定,运行期不变。Loop 通过 `provider.capability()` 拿到,主要用途:
1. CompactionPolicy.should_compact 算阈值(§10.3)
2. Provider adapter 自身的请求装配决策(如 reasoning 字段处理)

### 11.2 ProviderEvent(流式事件)

```rust
#[derive(Debug, Clone)]
pub enum ProviderEvent {
    /// 助手文本增量(任意长度)
    TextDelta(String),

    /// Reasoning 增量。signature 仅在 reasoning 段最后一个 delta 携带;中间为 None。
    /// 若整段流完仍无 signature,adapter 仍以 None 结束,Loop 构造 Reasoning block 时容忍。
    ReasoningDelta { text: String, signature: Option<String> },

    /// Tool use 增量。同一个 index 的多次 delta 需要 adapter 内部累积。
    /// adapter 在累积成合法 JSON 后,发出对应的 ToolUseComplete。
    ToolUseDelta {
        index: u32,
        id: Option<String>,         // 仅首个 delta 携带
        name: Option<String>,       // 仅首个 delta 携带
        args_delta: Option<String>, // 增量片段
    },

    /// 一次 tool use 累积完成
    ToolUseComplete {
        call_id: String,
        name: String,
        input: serde_json::Value,
    },

    /// 流正常结束
    Done { stop_reason: StopReason },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    Other,
}
```

**关键设计**:
- 没有 `ProviderEvent::Error` variant —— 流元素改为 `Result<ProviderEvent, ProviderError>`(见 §11.4),错误走标准 stream `Result` 模式,Loop 用 `try_next` 自然处理
- `ToolUseDelta` + `ToolUseComplete` 拆开:adapter 负责增量累积,Loop 永远不做 JSON 流式拼接
- Reasoning 的 signature 处理交给 adapter 兜底,Loop 只看到最终 delta

### 11.3 ProviderError(5 类分级)

```rust
#[derive(Debug, Clone, thiserror::Error)]
pub enum ProviderError {
    /// 上下文溢出。Loop 捕获后走 §10.7 单次兜底压缩 + 重试。
    #[error("context window exceeded: {0}")]
    ContextOverflow(String),

    /// 限流 / 配额。retry_after 来自 Retry-After header,无则 None。
    #[error("rate limited: {message}")]
    RateLimited { retry_after: Option<Duration>, message: String },

    /// 鉴权失败(401 / 403)。Loop 终止,不重试。
    #[error("auth failed: {0}")]
    Auth(String),

    /// 输入构造错误(400 且非 ContextOverflow)。通常是 bug,不重试。
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// 网络层 / 5xx / 协议解析错误。Loop 可决定单次重试。
    #[error("transient: {0}")]
    Transient(String),
}

impl ProviderError {
    /// 是否值得 Loop 自动一次性重试。ContextOverflow 不在此列(它走压缩兜底)。
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::Transient(_))
    }
}
```

**合并 Network 与 Transient**:Loop 对它们的处理一致(单次重试),区分无意义。

### 11.4 Provider trait

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    /// 模型 capability,运行期不变
    fn capability(&self) -> &Capability;

    /// 模型标识(给日志 / 事件用,不参与逻辑判断)
    fn model_id(&self) -> &str;

    /// 流式推理。
    ///
    /// - 外层 `Err`:开始流之前就失败(连接 / 鉴权 / 初始 4xx)
    /// - 流元素 `Err`:流开始后中途失败(5xx 中断 / 解析错 / context overflow 在生成中)
    /// - 流必须最终以一个 `Ok(ProviderEvent::Done { .. })` 元素结束,除非中途出 `Err`
    async fn stream(
        &self,
        prompt: &Prompt,
    ) -> Result<BoxStream<'_, Result<ProviderEvent, ProviderError>>, ProviderError>;

    /// 非流式一次性推理。仅供 §10.6 压缩摘要使用。
    /// 实现可以内部复用 stream,把所有 TextDelta 拼接返回。
    async fn complete_once(
        &self,
        prompt: &Prompt,
    ) -> Result<String, ProviderError>;
}
```

**契约要点**:

1. **取消由调用方负责**:Provider 不直接持有 `CancellationToken`。Loop 在外层 `select! { _ = stream.next() => ..., _ = cancel.cancelled() => ... }` 中监听,stream 被 drop 时 adapter 必须立即关闭底层 HTTP 连接(`reqwest-eventsource` drop 即关闭,默认满足)

2. **trait 仅为测试可替换**:V1 唯一生产实现是 `OpenAiProvider`(§11.5);`MockProvider` 用于 Loop / Compaction 单元测试。**不要**为想象中的多 provider 增加方法或字段

3. **`complete_once` 不带工具**:摘要请求不应该让模型走 tool use 路径,实现内部装配 prompt 时 `tools = []`

### 11.5 OpenAiProvider 实现要点

V1 唯一生产实现。本节是契约的一部分,不是"实现可以自由发挥"的注释。

#### 11.5.1 构造与配置

```rust
pub struct OpenAiProvider {
    api_key: String,
    base_url: String,         // 如 https://api.openai.com/v1
    model: String,            // 如 gpt-4o, o1-mini
    capability: Capability,
    client: reqwest::Client,
}

impl OpenAiProvider {
    pub fn new(api_key: String, base_url: String, model: String, capability: Capability) -> Self;
}
```

**Capability 由调用方传入**——v1 不做"根据 model 字符串自动推断 capability"的魔法。配置文件(§Config)负责。

#### 11.5.2 Prompt → OpenAI messages 翻译

按 Role 派发:

| 内部 Role | OpenAI role | 备注 |
|---|---|---|
| `Role::System` | `system` | content 拼成字符串 |
| `Role::User` | `user` | content 拼成字符串(V1 不传图,Image block 直接跳过) |
| `Role::Assistant` | `assistant` | Text blocks → `content`;ToolUse blocks → `tool_calls` 数组 |
| `Role::Tool` | `tool` (每个 ToolResult 一条独立 message) | OpenAI 要求每个 tool_result 是独立 message,**展开** |
| `Role::Summary` | `user` | content 加前缀 `<COMPACTION_SUMMARY>\n` |

**Reasoning block 处理**(关键):
- `capability.supports_reasoning == true` 时:Reasoning block 序列化为 OpenAI o-series 兼容的 reasoning 字段(具体字段名按 OpenAI 当时的协议——目前 o-series 的 reasoning 是返回字段而非输入字段,**输入侧丢弃即可**)
- `capability.supports_reasoning == false` 时:Reasoning block 直接跳过不发
- **结论**:V1 的 Reasoning 在请求侧一律不发(无论 capability),但允许在响应侧接收并存进 history。下一轮投影时由 §10.5 的压缩剥离规则 + 本节的"输入侧丢弃"双重保证不回传

**Tool message 展开规则**:

```rust
// 一条 Role::Tool message 含多个 ToolResult block:
// Message { role: Tool, content: [ToolResult{c1}, ToolResult{c2}] }
// 展开为 OpenAI 格式两条独立 message:
// [
//   { "role": "tool", "tool_call_id": "c1", "content": "<text>" },
//   { "role": "tool", "tool_call_id": "c2", "content": "<text>" },
// ]
```

ToolResult 内部嵌套的 `content: Vec<ContentBlock>` 按规则只能含 Text / Image。V1 不传图,所以**只取 Text blocks 拼接**。

#### 11.5.3 SSE 累积规则

OpenAI Chat Completions stream 协议:

- `data: {...}` 行 = 一个 chunk,JSON
- `data: [DONE]` 行 = 流结束
- 每 chunk 含 `choices[0].delta`(`content` / `tool_calls` / `finish_reason`)

adapter 必须实现以下累积逻辑:

```
1. 收到 delta.content 字符串 → 直接 emit ProviderEvent::TextDelta(s)
2. 收到 delta.tool_calls[i] 增量 →
   - 同 index 多次出现要内部累积 id / name / arguments
   - emit ProviderEvent::ToolUseDelta { index, id, name, args_delta }
   - 当某 index 的 finish_reason 出现(或全流 finish_reason 出现)时,
     对每个累积完成的 index,解析 arguments JSON,emit ToolUseComplete
   - arguments 解析失败 → 该 index 的 ToolUseComplete 用 input = {} 兜底
3. 收到 finish_reason → emit Done { stop_reason: 翻译表 }
4. 收到 [DONE] 行 → 流自然结束(若之前没 emit Done,补一个 Done { Other })
```

**stop_reason 翻译表**:

| OpenAI finish_reason | StopReason |
|---|---|
| `stop` | `EndTurn` |
| `tool_calls` | `ToolUse` |
| `length` | `MaxTokens` |
| 其他 / 缺失 | `Other` |

#### 11.5.4 错误码到 ProviderError 的映射

| 触发情形 | 映射 |
|---|---|
| HTTP 400,响应体 `error.code` 含 `context_length_exceeded` 或 message 含 `maximum context length` | `ContextOverflow(...)` |
| HTTP 400 其他 | `InvalidRequest(...)` |
| HTTP 401 / 403 | `Auth(...)` |
| HTTP 429 | `RateLimited { retry_after: 解析 Retry-After header, message: ... }` |
| HTTP 5xx / 网络错误 / `reqwest::Error` / SSE 解析错 | `Transient(...)` |
| 流中途连接断 | `Transient("stream interrupted: ...")` 走流元素 `Err` |
| 流中途响应 5xx | 同上 |
| 流中途收到 OpenAI 兼容服务的错误 chunk(部分国产服务在 SSE 内插入错误 JSON) | 按 message 模式判定 ContextOverflow / 其他 → 走流元素 `Err` |

**ContextOverflow 识别**:必须实现,§10.7 兜底依赖。OpenAI 错误体形如:
```json
{"error": {"code": "context_length_exceeded", "message": "..."}}
```
adapter 解析 `error.code` 优先;部分兼容服务不带 `code`,fallback 到 message 字符串匹配 `maximum context length` / `context length` / `too long`。

#### 11.5.5 复用 stream 实现 complete_once

```rust
async fn complete_once(&self, prompt: &Prompt) -> Result<String, ProviderError> {
    // 装配请求时强制 tools = []
    let stream_prompt = Prompt { tools: vec![], ..prompt.clone() };
    let mut stream = self.stream(&stream_prompt).await?;
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        match item? {
            ProviderEvent::TextDelta(s) => text.push_str(&s),
            ProviderEvent::Done { .. } => break,
            _ => {} // 忽略 Reasoning / ToolUse(理论上 tools=[] 不会出现)
        }
    }
    Ok(text)
}
```

也可以单独实现非流式版本以省一次 SSE 解析,但 V1 优先简单——复用 stream 一份代码。

### 11.6 V1 不做(明确红线)

- **多 Provider 抽象层**:不为 Anthropic / Bedrock / Gemini 设计接口位
- **图片输入**:V1 不传,Provider 不需要处理 ContentBlock::Image 的请求侧序列化
- **非流式主路径**:Loop 永远走 stream,complete_once 仅供压缩
- **Tool use 可选**:不支持 tool use 的模型不在范围内,Provider 实现进来不需要分支判断
- **自动 capability 推断**:Capability 由配置文件指定,不根据 model 字符串猜
- **多次重试 / 指数退避**:`is_retryable` 仅供 Loop 决定单次重试,V1 不做退避循环
- **Streaming usage 统计 / cost tracking**:V1 不收集 token usage 元数据(opencode / Claude Code 都做了,V1 留待后续)

### 11.7 不变量补充(并入 §8 Cheat Sheet)

| 不变量 | 守护点 |
|---|---|
| Capability 运行期不变 | `Provider::capability` 返回 `&Capability`(不可变借用) |
| 流元素错误统一走 Result | `ProviderEvent` 不含 Error variant |
| Tool use args 必须累积成合法 JSON 才发 ToolUseComplete | `OpenAiProvider` SSE 累积逻辑 |
| ContextOverflow 必须可识别 | `OpenAiProvider` §11.5.4 错误映射表 |
| complete_once 永远不带工具 | adapter 内部强制 `tools = []` |
| Reasoning 输入侧不回传 | adapter §11.5.2 Reasoning 处理规则 |
| stream drop 立即关闭连接 | 依赖 reqwest-eventsource 默认行为 |

---

## 12. Agent Loop

### 12.0 设计依据

参考三家:

| 维度 | codex | opencode | Claude Code |
|---|---|---|---|
| 主体结构 | `loop {}` 顺序 await | `while (openActivity)` | `while(true)` + State.transition 字段 |
| Approval IO | JSON-RPC request/response | Effect channel | **`canUseTool` 回调** |
| 重试 | RetryLimit + retry-after | maxRetries 配置 + 指数退避 | 多种(token 升级 / 模型 fallback) |
| 并行 tool 失败 | 共享 cancel token | eager + awaitAllSettled | StreamingToolExecutor + abortSignal |
| Compaction 失败 | 放弃本轮 | **保留旧 boundary** | 整轮 abort |
| Approval 粒度 | per-tool | per-tool | per-tool |

V1 取舍:
- **半显式状态机**(学 Claude Code):主体 `while` + `LoopTransition` enum 记录上轮原因,用 bool flag 防止无限兜底循环
- **回调式 ApprovalIO**(学 Claude Code):`Arc<dyn Fn(...) -> BoxFuture<bool>>`,比 trait 简单,比 mpsc 灵活
- **per-tool approval**(三家共识)
- **eager + await-all + 共享 child cancel token**(三家共识):并行执行,失败互不影响,user cancel 时全取消
- **Compaction 失败保留旧 boundary**(学 opencode):降级跳过,下轮再试
- **重试单次 + retry-after**:不做指数退避
- **MaxTokens 截断时升级 max_output 重试一次**(学 Claude Code):生产高频场景

### 12.1 LoopTransition

```rust
/// 记录上一轮迭代的转移原因。用于:
/// 1. 防止 Overflow / Compaction / MaxTokens 兜底无限循环
/// 2. 调试日志与事件追踪
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopTransition {
    /// 初始状态:第一次进入循环
    Initial,
    /// 上一轮模型正常给出 tool_use,本轮处理 tool 结果后回模型
    ToolResultReturn,
    /// 上一轮触发 Overflow,已兜底压缩并重试
    OverflowRetried,
    /// 上一轮 MaxTokens 截断,已升级 max_output 重试
    MaxTokensRetried,
    /// 上一轮可重试错误(Transient / RateLimited),已重试
    TransientRetried,
}

impl LoopTransition {
    /// 当前轮是否处于"已经为某种异常重试过"的状态——再次发生同类异常时直接放弃
    pub fn is_post_retry(&self) -> bool {
        matches!(self,
            Self::OverflowRetried | Self::MaxTokensRetried | Self::TransientRetried
        )
    }
}
```

**关键约束**:每种 retry 状态**只允许一次**——再触发同类异常立即终止。这是三家共识(opencode 的 `hasAttemptedReactiveCompact`、Claude Code 的同名 flag)。

### 12.2 ApprovalCallback

```rust
/// 用户审批回调。Loop 在每个 ToolCall 上调用一次(per-tool)。
///
/// 实现职责:
/// 1. emit Event::ApprovalRequired(可选,Loop 也会 emit)
/// 2. 询问用户(stdin / TUI / IPC)
/// 3. 返回 true=批准 / false=拒绝
///
/// 不需要处理 cancel:Loop 会在外层用 select! 监听 cancel token,
/// 回调被中途丢弃时实现侧的 future 自然 drop。
pub type ApprovalCallback = Arc<
    dyn Fn(&ToolCall, &RunContext) -> BoxFuture<'static, bool>
    + Send + Sync
>;

/// 默认实现:Yolo 模式自动 true,不调用回调
pub fn yolo_approval() -> ApprovalCallback {
    Arc::new(|_, _| Box::pin(async { true }))
}

/// 测试用:固定返回值
pub fn const_approval(answer: bool) -> ApprovalCallback {
    Arc::new(move |_, _| Box::pin(async move { answer }))
}
```

**为什么不用 trait**:
- 单方法 trait 在 Rust 里基本等同于函数指针/闭包,trait 只多一层抽象
- 回调式更易组合(可以包多层中间件 / 日志 / 事件 emit)
- Claude Code 同款思路

**ApprovalCallback 与 ApprovalGate 的关系**(回顾 §9.7):
1. Loop 拿到 ToolCall 后先问 `ApprovalGate.decide(risk, advice)`
2. Gate 返回 `AutoApprove` → 跳过 callback,直接执行
3. Gate 返回 `Ask` → 调用 `ApprovalCallback`
4. Gate 在 `MustAsk` 下即使 Yolo 也走 Ask 分支(回调实现可以是"yolo 默认 true",但 MustAsk 强制问真用户)

V1 实现层面,`yolo_approval()` 实际上是"无视 MustAsk 也批准"——这与 §9.7 的契约冲突。**正确做法**:Yolo 模式下 ApprovalGate 仍然返回 `Ask`(对 MustAsk 来说),Loop 调用注入的 callback。CLI 在 Yolo 模式下应该传一个**只对 MustAsk 提示用户、其他自动批准**的 callback,而不是无脑的 `yolo_approval`。

为简化 V1,约定:
- `--mode yolo`:Loop 收到 `Decision::Ask` 仍调用 callback,callback 由 CLI 决定怎么处理(默认 true,但实现可以对 MustAsk 走 stdin)
- `--mode default`:Loop 收到 `Decision::Ask` 调用 callback,callback 走 stdin 提问

### 12.3 RunContext

把 Loop 一轮 send 需要的所有依赖打包,避免参数列表臃肿。

```rust
pub struct RunContext {
    /// Session 标识
    pub session_id: String,

    /// 审批模式
    pub approval_mode: ApprovalMode,

    /// 工作目录(传给 ExecutionContext)
    pub cwd: PathBuf,

    /// 工具默认超时
    pub tool_timeout: Duration,

    /// 工具最大输出字节数
    pub tool_max_output_bytes: usize,

    /// 本轮 send 的 cancel token(由 Session::send 内部从 cancel_root 派生)
    pub cancel: CancellationToken,

    /// Sink 句柄
    pub sink: Arc<dyn EventSink>,
}
```

**关键约束**:
- `cancel` 是 Session::send 入口处从 `cancel_root.child_token()` 派生的——这样上一轮的 cancel 不影响本轮(修上一轮审计 #1)
- Loop 内部用 `cancel.child_token()` 再派生 child 给 tool 并发组,这样**user cancel root 时整组工具同时取消**(Q9 要求)

### 12.4 主循环结构

```rust
pub async fn run_loop(
    session: &mut Session,
    user_input: Vec<ContentBlock>,
    provider: &dyn Provider,
    registry: &ToolRegistry,
    compaction_policy: &dyn CompactionPolicy,
    approval_callback: ApprovalCallback,
    ctx: &RunContext,
) -> Result<(), Error> {
    session.history.push_user(user_input);

    let mut transition = LoopTransition::Initial;
    let mut turns = 0_usize;
    let mut max_output_override: Option<usize> = None;

    loop {
        // ---- 0. 早期 cancel 检查 ----
        if ctx.cancel.is_cancelled() {
            return finalize_cancelled(session, ctx, "before model call").await;
        }

        // ---- 1. 压缩(可降级跳过)----
        if let Err(e) = maybe_compact(
            &mut session.history, compaction_policy, provider,
            provider.capability(), ctx.sink.clone(), &ctx.session_id
        ).await {
            // 压缩失败:emit Error event,保留旧 boundary,继续本轮(降级)
            ctx.sink.emit(Event::Error {
                session_id: ctx.session_id.clone(),
                message: format!("compaction skipped: {e}"),
            }).await;
        }

        // ---- 2. 装配 prompt + 流式调用模型 ----
        let prompt = build_prompt(session, registry);
        let stream_result = stream_model(
            &prompt, provider, &ctx.cancel, &ctx.session_id,
            ctx.sink.clone(), max_output_override,
        ).await;

        let (assistant_blocks, stop_reason) = match stream_result {
            Ok(r) => r,

            // ---- 3a. ContextOverflow:单次兜底 ----
            Err(StreamError::Provider(ProviderError::ContextOverflow(_)))
                if !matches!(transition, LoopTransition::OverflowRetried) =>
            {
                overflow_compact(
                    &mut session.history, compaction_policy, provider,
                    ctx.sink.clone(), &ctx.session_id
                ).await?;
                transition = LoopTransition::OverflowRetried;
                continue;
            }

            // ---- 3b. 可重试错误:单次重试 ----
            Err(StreamError::Provider(e))
                if e.is_retryable() && !matches!(transition, LoopTransition::TransientRetried) =>
            {
                if let ProviderError::RateLimited { retry_after: Some(d), .. } = &e {
                    tokio::time::sleep(*d).await;
                }
                transition = LoopTransition::TransientRetried;
                continue;
            }

            // ---- 3c. Cancel ----
            Err(StreamError::Cancelled) => {
                return finalize_cancelled(session, ctx, "model streaming").await;
            }

            // ---- 3d. 其他错误一律上抛 ----
            Err(StreamError::Provider(e)) => return Err(Error::Provider(e)),
        };

        // ---- 4. MaxTokens 截断兜底 ----
        if stop_reason == StopReason::MaxTokens
            && !matches!(transition, LoopTransition::MaxTokensRetried)
        {
            // 升级 max_output 重试本轮(不入 history)
            max_output_override = Some(provider.capability().max_output * 4);
            transition = LoopTransition::MaxTokensRetried;
            continue;
        }
        max_output_override = None; // 任何成功路径都重置

        // ---- 5. assistant message 入 history ----
        session.history.push_assistant(assistant_blocks.clone());

        // ---- 6. 终止判定 ----
        let tool_calls = extract_tool_uses(&assistant_blocks);
        if tool_calls.is_empty() {
            return Ok(()); // Done
        }

        // ---- 7. 进入工具阶段前的 turn 限制 ----
        turns += 1;
        if turns >= MAX_TURNS {
            ctx.sink.emit(Event::Error {
                session_id: ctx.session_id.clone(),
                message: "max turns exceeded".into(),
            }).await;
            return Err(Error::MaxTurnsExceeded);
        }

        // ---- 8. Approval + 工具执行 ----
        let outcomes = match run_tool_phase(
            &tool_calls, registry, &approval_callback, ctx
        ).await {
            ToolPhaseOutcome::Executed(results) => results,
            ToolPhaseOutcome::Cancelled(partial_results) => {
                // user cancel:partial_results 含已完成 + cancelled 的混合
                session.history.push_tool_results(partial_results)?;
                return finalize_cancelled(session, ctx, "tool execution").await;
            }
            ToolPhaseOutcome::AllRejected(rejection_results) => {
                // 全部被拒:回填 tool_result(is_error=true),回模型让它自决
                session.history.push_tool_results(rejection_results)?;
                transition = LoopTransition::ToolResultReturn;
                continue;
            }
        };

        // ---- 9. tool_results 入 history,回模型 ----
        session.history.push_tool_results(outcomes)?;
        transition = LoopTransition::ToolResultReturn;
    }
}

const MAX_TURNS: usize = 50;
```

**入口顺序解释**(对照 Q4 决策):
- step 1 压缩:每轮入口检查,可能触发 → 模型调用前 history 已收紧
- step 2 模型流式调用
- step 3 流错误的三种兜底:Overflow / Transient / Cancel
- step 4 MaxTokens 截断升级重试
- step 5 推 assistant
- step 6 无 tool_use → Done
- **step 7 turn 计数+1 + MAX_TURNS 检查**:仅在确认有 tool_use 之后,**纯文本回复不算 turn**(语义更准)
- step 8 ApprovalGate + 并行执行
- step 9 推 tool_results,回 step 0

### 12.5 stream_model

```rust
async fn stream_model(
    prompt: &Prompt,
    provider: &dyn Provider,
    cancel: &CancellationToken,
    session_id: &str,
    sink: Arc<dyn EventSink>,
    max_output_override: Option<usize>,
) -> Result<(Vec<ContentBlock>, StopReason), StreamError> {
    sink.emit(Event::AssistantMessageStart {
        session_id: session_id.to_owned(),
    }).await;

    let stream_result = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(StreamError::Cancelled),
        s = provider.stream(prompt) => s,
    };
    let mut stream = stream_result.map_err(StreamError::Provider)?;

    let mut blocks: Vec<ContentBlock> = Vec::new();
    let mut text_buf = String::new();
    let mut reasoning_buf: Option<(String, Option<String>)> = None;
    let mut stop_reason = StopReason::Other;

    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(StreamError::Cancelled),
            n = stream.next() => n,
        };

        let event = match next {
            None => break,
            Some(Err(e)) => return Err(StreamError::Provider(e)),
            Some(Ok(ev)) => ev,
        };

        match event {
            ProviderEvent::TextDelta(t) => {
                text_buf.push_str(&t);
                sink.emit(Event::AssistantToken {
                    session_id: session_id.to_owned(),
                    text: t,
                }).await;
            }
            ProviderEvent::ReasoningDelta { text, signature } => {
                let entry = reasoning_buf.get_or_insert((String::new(), None));
                entry.0.push_str(&text);
                if signature.is_some() { entry.1 = signature; }
            }
            ProviderEvent::ToolUseDelta { .. } => {
                // Loop 不消费增量,仅等 ToolUseComplete
            }
            ProviderEvent::ToolUseComplete { call_id, name, input } => {
                // 在 ToolUse 之前先固化 text / reasoning(如果有)
                flush_text_and_reasoning(&mut blocks, &mut text_buf, &mut reasoning_buf);
                blocks.push(ContentBlock::ToolUse { call_id, name, input });
            }
            ProviderEvent::Done { stop_reason: sr } => {
                stop_reason = sr;
                break;
            }
        }
    }
    flush_text_and_reasoning(&mut blocks, &mut text_buf, &mut reasoning_buf);

    sink.emit(Event::AssistantMessageEnd {
        session_id: session_id.to_owned(),
    }).await;
    Ok((blocks, stop_reason))
}

fn flush_text_and_reasoning(
    blocks: &mut Vec<ContentBlock>,
    text_buf: &mut String,
    reasoning_buf: &mut Option<(String, Option<String>)>,
) {
    if !text_buf.is_empty() {
        blocks.push(ContentBlock::Text { text: std::mem::take(text_buf) });
    }
    if let Some((text, signature)) = reasoning_buf.take() {
        if !text.is_empty() {
            blocks.push(ContentBlock::Reasoning { text, signature });
        }
    }
}

#[derive(Debug)]
enum StreamError {
    Cancelled,
    Provider(ProviderError),
}
```

**关键约束**:
- 块顺序:同一段流里,Text → ToolUse 的相对顺序保留(每次 ToolUseComplete 来之前先 flush Text)
- Cancel 路径**不 push 任何 block 进 history**——assistant_blocks 直接丢弃(对应 §3 状态图情况①)
- AssistantMessageStart / AssistantMessageEnd 事件配对:cancel 路径下也补一个 End,保持 sink 流自洽(修上一轮审计 #9)。这里实现细节:`finalize_cancelled` 内部补 End。

### 12.6 工具阶段(run_tool_phase)

```rust
enum ToolPhaseOutcome {
    /// 全部工具完成执行(成功 / 失败 / 拒绝结果都打包进 results,role=Tool message 直接入 history)
    Executed(Vec<ContentBlock>),
    /// 用户取消:partial_results 含已完成 + cancelled 的混合,Loop 入 history 后走 cancel 路径
    Cancelled(Vec<ContentBlock>),
    /// 全部被拒(ApprovalCallback 全返回 false):rejection_results 是 N 条 ToolResult(is_error=true)
    AllRejected(Vec<ContentBlock>),
}

async fn run_tool_phase(
    calls: &[ToolCall],
    registry: &ToolRegistry,
    approval: &ApprovalCallback,
    ctx: &RunContext,
) -> ToolPhaseOutcome {
    // ---- 1. per-tool approval(顺序执行,不并发问) ----
    let mut approved: Vec<&ToolCall> = Vec::new();
    let mut rejected: Vec<&ToolCall> = Vec::new();

    for call in calls {
        let tool = registry.get(&call.name);
        let advice = match tool {
            Some(t) => t.approval_advice(&call.input),
            None => ToolApprovalAdvice::default_for(RiskLevel::Dangerous),
        };
        let risk = tool.map(|t| t.spec().risk).unwrap_or(RiskLevel::Dangerous);

        let decision = ApprovalGate { mode: ctx.approval_mode }.decide(risk, advice);
        let approved_this = match decision {
            ApprovalOutcome::AutoApprove => {
                ctx.sink.emit(Event::ApprovalGranted {
                    session_id: ctx.session_id.clone(),
                    call_id: call.call_id.clone(),
                }).await;
                true
            }
            ApprovalOutcome::Ask => {
                ctx.sink.emit(Event::ApprovalRequired {
                    session_id: ctx.session_id.clone(),
                    call_id: call.call_id.clone(),
                    command: summarize_input(call),
                }).await;
                let result = (approval)(call, ctx).await;
                ctx.sink.emit(if result {
                    Event::ApprovalGranted { session_id: ctx.session_id.clone(), call_id: call.call_id.clone() }
                } else {
                    Event::ApprovalRejected { session_id: ctx.session_id.clone(), call_id: call.call_id.clone() }
                }).await;
                result
            }
        };

        if approved_this { approved.push(call); }
        else { rejected.push(call); }
    }

    // ---- 2. 全部被拒:走 AllRejected 分支 ----
    if approved.is_empty() {
        let results = rejected.into_iter().map(|c|
            ContentBlock::ToolResult {
                call_id: c.call_id.clone(),
                content: vec![ContentBlock::Text {
                    text: "user rejected this tool call".into()
                }],
                is_error: true,
            }
        ).collect();
        return ToolPhaseOutcome::AllRejected(results);
    }

    // ---- 3. 并发执行 approved 工具,共享 child cancel token ----
    let group_cancel = ctx.cancel.child_token();
    let mut futs = Vec::with_capacity(approved.len());
    for call in &approved {
        let tool = match registry.get(&call.name) {
            Some(t) => t.clone(),
            None => {
                // 未知工具:直接产 failure,不进 invoke
                let block = ContentBlock::ToolResult {
                    call_id: call.call_id.clone(),
                    content: vec![ContentBlock::Text {
                        text: format!("unknown tool: {}", call.name),
                    }],
                    is_error: true,
                };
                ctx.sink.emit(Event::ToolError {
                    session_id: ctx.session_id.clone(),
                    call_id: call.call_id.clone(),
                    error: format!("unknown tool: {}", call.name),
                }).await;
                futs.push(Box::pin(async move { block }) as BoxFuture<'_, ContentBlock>);
                continue;
            }
        };

        let exec_ctx = ExecutionContext {
            session_id: ctx.session_id.clone(),
            call_id: call.call_id.clone(),
            cwd: ctx.cwd.clone(),
            timeout: ctx.tool_timeout,
            max_output_bytes: ctx.tool_max_output_bytes,
            cancel: group_cancel.clone(),
            sink: ctx.sink.clone(),
        };
        let call = (*call).clone();
        let sink = ctx.sink.clone();
        let session_id = ctx.session_id.clone();
        futs.push(Box::pin(async move {
            invoke_tool(&*tool, call, &exec_ctx, sink, session_id).await
        }) as BoxFuture<'_, ContentBlock>);
    }

    let results = futures::future::join_all(futs).await;

    // ---- 4. 拼接 rejection + execution 结果(保持原顺序)----
    let merged = merge_in_call_order(calls, &rejected, &approved, results);

    // ---- 5. 判断是不是 user cancel ----
    if ctx.cancel.is_cancelled() {
        ToolPhaseOutcome::Cancelled(merged)
    } else {
        ToolPhaseOutcome::Executed(merged)
    }
}
```

**关键约束**:
- **Approval 顺序、execution 并发**:approval 必须串行(用户依次回答),execute 完全并发(eager + await all)
- **共享 child cancel token**:`group_cancel = ctx.cancel.child_token()` 让 user cancel 时整组工具同时取消(Q9)
- **未知工具不 panic**:直接生成 failure ToolResult(修上一轮审计 #4)
- **结果顺序还原**:虽然 execute 并发,但 history 中的 ToolResult 顺序必须与原 ToolUse 顺序一致——`merge_in_call_order` 按 call_id 重排

### 12.7 invoke_tool

把 §9.9 的契约具体化:

```rust
async fn invoke_tool(
    tool: &dyn Tool,
    call: ToolCall,
    ctx: &ExecutionContext,
    sink: Arc<dyn EventSink>,
    session_id: String,
) -> ContentBlock {
    sink.emit(Event::ToolStart {
        session_id: session_id.clone(),
        call_id: call.call_id.clone(),
        tool: call.name.clone(),
        input: call.input.clone(),
    }).await;

    let started = std::time::Instant::now();

    // 框架兜底:timeout + cancel
    let outcome = tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => ToolOutput::cancelled(),
        out = tokio::time::timeout(ctx.timeout, tool.run(call.input.clone(), ctx)) => match out {
            Ok(o) => o,
            Err(_) => ToolOutput::failure(format!("timeout after {:?}", ctx.timeout)),
        },
    };

    let duration_ms = started.elapsed().as_millis() as u64;

    // emit 完成事件
    if outcome.is_error {
        let err_text = first_text(&outcome.content).unwrap_or_default();
        sink.emit(Event::ToolError {
            session_id, call_id: call.call_id.clone(), error: err_text,
        }).await;
    } else {
        let output_value = blocks_to_json(&outcome.content);
        sink.emit(Event::ToolEnd {
            session_id, call_id: call.call_id.clone(),
            output: output_value, duration_ms,
        }).await;
    }

    // 转 ContentBlock::ToolResult
    ContentBlock::ToolResult {
        call_id: call.call_id,
        content: outcome.content,
        is_error: outcome.is_error,
    }
}
```

**与 §9.9 契约的关系**:本节的 `invoke_tool` 是 §9.9 的具体实现。`ApprovalGate` 决策已在 §12.6 完成,此处不再做。

### 12.8 finalize_cancelled

每次 cancel 路径退出 Loop 前,统一走这个函数:

```rust
async fn finalize_cancelled(
    session: &mut Session,
    ctx: &RunContext,
    reason: &str,
) -> Result<(), Error> {
    // sink 完整性:补一个 AssistantMessageEnd(若上轮 stream 中途 cancel)
    // 这里通过 RunContext 的标志位判断,实现细节略——核心是保证 sink 流配对
    ctx.sink.emit(Event::Cancelled {
        session_id: ctx.session_id.clone(),
        reason: reason.to_owned(),
    }).await;
    Ok(())
}
```

**约束**:
- 本函数**不修改 history**——三种 cancel 场景的 history 修复义务在调用 `finalize_cancelled` 前已经完成

| 场景 | history 修复时机 | run_loop 调用点 |
|---|---|---|
| ① 流式中 cancel | 不修复(history 干净) | step 3c |
| ② tool cancel | partial_results 已 push | ToolPhaseOutcome::Cancelled 分支 |
| ③ 入口 cancel | 不修复 | step 0 |

### 12.9 错误传播总览

| 错误来源 | Loop 处理 | 上抛 |
|---|---|---|
| `ctx.cancel` 触发(任意位置) | emit `Cancelled` event,return Ok | ❌ |
| `MAX_TURNS` 超限 | emit `Error` event,return `Err(MaxTurnsExceeded)` | ✅ |
| `ProviderError::ContextOverflow` 首次 | overflow_compact 兜底,设置 `OverflowRetried`,continue | ❌ |
| `ProviderError::ContextOverflow` 兜底后再次 | 上抛 | ✅ |
| `ProviderError::Auth` | 上抛 | ✅ |
| `ProviderError::InvalidRequest` | 上抛 | ✅ |
| `ProviderError::RateLimited` 首次 | 尊重 retry_after,设置 `TransientRetried`,continue | ❌ |
| `ProviderError::Transient` 首次 | 设置 `TransientRetried`,continue | ❌ |
| `ProviderError::*` 重试后再次 | 上抛 | ✅ |
| `StopReason::MaxTokens` 首次 | 升级 max_output,设置 `MaxTokensRetried`,continue | ❌ |
| `StopReason::MaxTokens` 重试后再次 | 接受当前 assistant_blocks,正常入 history,继续循环 | ❌ |
| Compaction 失败(非 Overflow) | emit Error event,跳过本次压缩,继续本轮 | ❌ |
| Tool 抛异常 / 超时 | 框架兜底为 ToolOutput::failure,写回 tool_result | ❌ |
| 未知工具名 | 写回 tool_result(is_error=true) | ❌ |
| `HistoryError`(配对违反) | panic | ⚠️ 内部 bug |

**MaxTokens 重试后再次截断的处理**:不再升级,接受当前已截断的 assistant content,push 进 history 并继续循环——让模型在下一轮基于已截断输出继续(可能产出 "继续上面..." 的 follow-up)。

### 12.10 Session::send 的简化

```rust
impl Session {
    pub async fn send(
        &mut self,
        user_input: Vec<ContentBlock>,
        provider: &dyn Provider,
        registry: &ToolRegistry,
        compaction_policy: &dyn CompactionPolicy,
        approval_callback: ApprovalCallback,
        config: &AgentConfig,
    ) -> Result<(), Error> {
        // 从 root 派生本轮专属 cancel token,上一轮的 cancel 不影响本轮
        let cancel = self.cancel_root.child_token();

        let ctx = RunContext {
            session_id: self.session_id.clone(),
            approval_mode: config.approval_mode,
            cwd: config.cwd.clone(),
            tool_timeout: config.tool_timeout,
            tool_max_output_bytes: config.tool_max_output_bytes,
            cancel,
            sink: self.sink.clone(),
        };

        run_loop(self, user_input, provider, registry,
                 compaction_policy, approval_callback, &ctx).await
    }

    /// 用户主动取消当前 send 的快捷接口
    pub fn cancel_current(&self) {
        self.cancel_root.cancel();
    }
}
```

**关键变化**(对照上一轮审计 #1):
- `Session.cancel_root` 永久持有,**不重置**
- 每次 send 内部 `child_token()` 派生本轮专属 token
- `cancel_current()` 取消 root,但**下一次 send 会派生新的 child token,自动恢复**——多轮 ctrl-c 才能持续生效
- 等价的 ctrl-c handler:`tokio::spawn({ let s = session.clone(); async move { ctrl_c().await; s.cancel_current(); } })`

### 12.11 V1 不做(明确红线)

- **指数退避**:`is_retryable` 仅决定单次重试,不做 backoff 循环
- **多模型 fallback**:Claude Code 有,V1 不做
- **流式 tool result**:tool 返回是单次 ToolOutput
- **Subagent / Task delegation**:V1 单 agent 单 loop
- **Stop hooks / pre-tool / post-tool hooks**:V1 不留扩展点(实现细节都在 invoke_tool 内,可以以后再加)
- **持久化 RunContext**:RunContext 仅本轮活,不进 sink、不参与 replay
- **Custom transition states**:LoopTransition 5 个 variant 写死,不做 plugin 扩展

### 12.12 不变量补充(并入 §8 Cheat Sheet)

| 不变量 | 守护点 |
|---|---|
| 每种 retry 状态只生效一次 | `LoopTransition` flag + 主循环 match guard |
| Approval 是 per-tool 串行,Execute 是并发 | run_tool_phase 两阶段 |
| Tool 执行组共享 child cancel,user cancel 时整组取消 | `group_cancel = ctx.cancel.child_token()` |
| 未知工具不 panic | 直接生成 failure ToolResult |
| AssistantMessageStart / End 配对 | finalize_cancelled 兜底 |
| MAX_TURNS 仅计 tool execution 后回模型那一次 | step 7 在 tool_calls 非空时才 +1 |
| Compaction 失败保留旧 boundary | maybe_compact 失败时不调 record_compaction |
| Cancel 不修改 history | 各调用点已先修复 history,finalize_cancelled 只 emit |
| ToolResult 顺序与原 ToolUse 顺序一致 | merge_in_call_order 按 call_id 重排 |

---

## 13. System Prompt

### 13.0 设计依据

参考三家:

| 维度 | codex | opencode | Claude Code |
|---|---|---|---|
| 静态部分载体 | `gpt_5_x_prompt.md` 等模板文件 | `default.txt / kimi.txt / ...` 按 provider 选 | TS 函数返回常量字符串,分静态段/动态段 |
| 动态部分位置 | system 段(cwd / date / 工具列表) | system 段(env facts / date) | system 段 + boundary marker(prompt cache) |
| 用户自定义 | AGENTS.md | AGENTS.md / agent prompt 配置 | CLAUDE.md(项目)+ ~/.claude/CLAUDE.md(全局),注入到 **userContext** |
| 运行时提醒 | 无显式机制 | mid-conversation system message | **`<system-reminder>` 标签** 在 user message |
| 装配时机 | 每 turn | 每 turn(safe boundary) | 静态每 session,动态每 turn |
| 模型/Provider 绑定 | 模板按 model 版本绑定 | 按 provider 选模板 | 单一模板 |

V1 取舍:
- **静态部分写 .md 文件**(三家共识),不在 Rust 源码里写多页 prompt
- **动态部分塞 system 段**:V1 没有"运行时变化"的信息,简化成 system 段拼接;不为 V1 引入 `<system-reminder>` 机制
- **每 send 重建**:简单确定,日期 / 工具列表变化能反映
- **不分模型 / Provider**:V1 一份模板搞定
- **不做 AGENTS.md**:留 v1.x;V1 也不做 `~/.flash/instructions.md`
- **`<system-reminder>` 预留语义**:Message 类型不限制 user content 内容,任何人想塞都能塞;V1 框架不主动产生

### 13.1 SystemPrompt 装配函数

```rust
/// 装配 system 段消息列表。每次 Session::send 入口调用一次。
///
/// 返回 `Vec<Message>`,所有 message 的 role 必须是 Role::System。
pub fn build_system_prompt(
    static_template: &str,           // §13.2 加载的静态文本
    env: &EnvironmentSnapshot,        // §13.3 当前环境信息
    registry: &ToolRegistry,          // 工具列表(取名字 + 简要)
) -> Vec<Message> {
    let dynamic = render_environment(env, registry);
    vec![
        Message::system_text(static_template),
        Message::system_text(dynamic),
    ]
}
```

**为什么拆两条 system message**:
- 第一条静态文本,内容稳定,可以未来对接 prompt cache(哪天接 Anthropic 时不用动)
- 第二条动态内容,每轮可能不同
- OpenAI / Anthropic 都允许多条 system message,语义等价于拼接,但拆开调试更清楚

### 13.2 静态模板

文件位置:`prompts/system_default.md`(项目根 `prompts/` 目录)

加载方式:启动期一次性 `include_str!("../prompts/system_default.md")`,**编译进二进制**——避免运行时缺文件错误,也方便分发。

模板内容大纲(具体文本由实现时撰写,本节锁内容范围):

```markdown
# Identity
You are flash-code, an interactive CLI coding agent. You help users with software
engineering tasks by reading their code, running shell commands, and editing files.

# Tool usage
- Use tools when you need to inspect or modify the user's environment.
- Prefer reading files before editing them.
- Chain tool calls: don't ask the user for information you can obtain via tools.
- If a tool fails, read the error and decide whether to retry, try a different
  approach, or report back to the user.
- Don't include tool argument JSON in your text response — that's what tool calls
  are for.

# Output style
- Be concise. Match response length to task complexity: simple questions get short
  answers, complex tasks may warrant explanation.
- When referencing code, use `path/to/file.rs:42` format so the user can navigate.
- Avoid sycophantic openers ("Great question!") and closing fluff ("Hope this helps!").
- Use Markdown sparingly — code blocks for code, plain text otherwise.

# Safety
- Never commit, push, or perform destructive git operations unless the user
  explicitly asks.
- Never run `rm -rf` or similar destructive commands without confirmation.
- If a task is ambiguous, ask one clarifying question before acting on a
  potentially wrong assumption.

# Environment
The next system message contains your current environment (cwd, date, available
tools). Treat it as ground truth for the current session.
```

**约束**:
- 不在静态模板里列具体工具名(那部分由动态段处理 §13.3)
- 不在静态模板里写日期 / cwd(同上)
- 文本中文 / 英文不混用——目前 v1 用英文(模型对英文 system 段响应更稳定;用户输入语言不影响 system 段)

### 13.3 动态段内容

```rust
pub struct EnvironmentSnapshot {
    pub os: String,           // "darwin" / "linux" / "windows"
    pub shell: String,         // "zsh" / "bash" / "powershell"
    pub cwd: PathBuf,
    pub date: String,          // ISO 8601, "2026-06-25"
}

impl EnvironmentSnapshot {
    pub fn capture(cwd: PathBuf) -> Self {
        Self {
            os: std::env::consts::OS.to_owned(),
            shell: std::env::var("SHELL").unwrap_or_else(|_| "sh".into()),
            cwd,
            date: chrono::Local::now().format("%Y-%m-%d").to_string(),
        }
    }
}

fn render_environment(env: &EnvironmentSnapshot, registry: &ToolRegistry) -> String {
    let tool_list = registry.specs().iter()
        .map(|s| format!("- {}: {}", s.name, s.description))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "# Environment\n\
         OS: {}\n\
         Shell: {}\n\
         CWD: {}\n\
         Date: {}\n\
         \n\
         # Available tools\n\
         {}",
        env.os,
        env.shell,
        env.cwd.display(),
        env.date,
        tool_list,
    )
}
```

**为什么动态段也包含工具列表**:
- 模型从 `tools` 数组拿到的是结构化 schema(给 function calling 用)
- system 段里再列一次"工具名 + 描述",**模型会更稳定地选对工具**——这是 codex / Claude Code 的经验
- 不重复 schema 细节,只列 name + description,避免 system 段过大

**V1 不做的动态字段**:
- git branch / git status:V1 静态注入一次的成本与收益不匹配,等真有用户场景再加
- 用户名 / hostname:不必要
- 终端宽度 / 颜色支持:tool 自己处理

### 13.4 调用时机

每次 `Session::send` 入口装配,作为 §12.4 主循环 step 0 之前的一步:

```rust
impl Session {
    pub async fn send(
        &mut self,
        user_input: Vec<ContentBlock>,
        provider: &dyn Provider,
        registry: &ToolRegistry,
        compaction_policy: &dyn CompactionPolicy,
        approval_callback: ApprovalCallback,
        config: &AgentConfig,
    ) -> Result<(), Error> {
        let cancel = self.cancel_root.child_token();

        // 装配 system prompt
        let env = EnvironmentSnapshot::capture(config.cwd.clone());
        self.system = build_system_prompt(STATIC_TEMPLATE, &env, registry);

        let ctx = RunContext { /* ... */ };
        run_loop(self, user_input, provider, registry,
                 compaction_policy, approval_callback, &ctx).await
    }
}

const STATIC_TEMPLATE: &str = include_str!("../../prompts/system_default.md");
```

**关键约束**:
- `Session.system` 字段被**每次 send 整体覆写**——不增量、不持久
- system 装配**永远不参与 history.messages**——它只在每轮装配 Prompt 时被读取(§7.2 Loop 装配 prompt 流程)
- 装配函数纯函数,不 emit 事件,不依赖 sink

### 13.5 V1 不做(明确红线)

- **AGENTS.md / FLASH.md 项目级用户自定义**:留 v1.x,届时按 AGENTS.md 标准实现(对齐 codex / cursor / aider 生态)
- **`~/.flash/instructions.md` 全局用户自定义**:留 v1.x
- **`<system-reminder>` 主动产生**:V1 不做,但 Message 类型不限制内容——任何调用方需要时可以塞;V1 框架不主动产生
- **`<project-instructions>` 注入到 user message**:Claude Code 同款机制,V1 不需要
- **prompt cache boundary marker**:Anthropic 才需要,V1 OpenAI 兼容不做
- **多模型 / 多 Provider 分支模板**:codex / opencode 做,V1 一份模板搞定
- **运行时变化的 cwd / git status 注入**:V1 cwd 不变,git 不主动注入
- **从 `tools` 数组之外重复完整 ToolSpec**:动态段只列 name + description,不复制 input_schema

### 13.6 不变量补充(并入 §8 Cheat Sheet)

| 不变量 | 守护点 |
|---|---|
| Session.system 仅含 Role::System message | `build_system_prompt` 构造 |
| 静态模板编译进二进制 | `include_str!` |
| system 装配每 send 重做一次 | `Session::send` 入口 |
| 动态段不含 git / 用户名 / 终端信息 | EnvironmentSnapshot 字段限定 |
| system 不污染 history.messages | system 仅作为 Prompt 装配输入,从不 push 进 history |

---

## 14. 待办

数据 / 状态 / 编排 / 提示层 spec 完整闭环。下一步是实现工作,不再有 spec 待办。

实现起步建议从 §1-§3(数据模型)开始,然后 §11(Provider)、§9(Tool)、§10(Compaction)并行,最后 §12(Loop)+ §13(SystemPrompt)收口。### 12.8 finalize_cancelled

每次 cancel 路径退出 Loop 前,统一走这个函数:

```rust
async fn finalize_cancelled(
    session: &mut Session,
    ctx: &RunContext,
    reason: &str,
) -> Result<(), Error> {
    ctx.sink.emit(Event::Cancelled {
        session_id: ctx.session_id.clone(),
        reason: reason.to_owned(),
    }).await;
    Ok(())
}
```

**重要约束**:本函数**不修改 history**——三种 cancel 场景的 history 修复义务在调用 `finalize_cancelled` 前已经完成(§3 状态图):

| 场景 | history 修复时机 | 调用点 |
|---|---|---|
| ① 流式中 cancel | history 干净,无需修复 | run_loop step 3c |
| ② tool cancel | partial_results 已 push | run_loop ToolPhaseOutcome::Cancelled 分支 |
| ③ 早期 cancel | history 干净 | run_loop step 0 |

### 12.9 错误传播总览

| 错误来源 | Loop 处理 | 上抛 / 不上抛 |
|---|---|---|
| `ctx.cancel` 触发 | emit `Cancelled` event,return Ok | ❌ |
| `MAX_TURNS` 超限 | emit `Error` event,return `Err(MaxTurnsExceeded)` | ✅ |
| `ProviderError::ContextOverflow` 首次 | overflow_compact 兜底,设置 `OverflowRetried`,continue | ❌ |
| `ProviderError::ContextOverflow` 重试后再次 | `Err(Provider(ContextOverflow))` 上抛 | ✅ |
| `ProviderError::Auth` | `Err(Provider(Auth))` 上抛 | ✅ |
| `ProviderError::InvalidRequest` | `Err(Provider(InvalidRequest))` 上抛 | ✅ |
| `ProviderError::RateLimited` 首次 | 尊重 retry-after,设置 `TransientRetried`,continue | ❌ |
| `ProviderError::Transient` 首次 | 设置 `TransientRetried`,continue | ❌ |
| `ProviderError::*` 重试后再次 | 上抛 | ✅ |
| `StopReason::MaxTokens` 首次 | 升级 max_output,设置 `MaxTokensRetried`,continue | ❌ |
| `StopReason::MaxTokens` 重试后再次 | 接受当前 assistant_blocks,正常入 history,继续循环 | ❌ |
| Compaction 失败(非 Overflow) | emit Error event,跳过本次压缩,继续本轮 | ❌ |
| Tool 抛异常 / 超时 | 框架兜底为 ToolOutput::failure,写回 tool_result | ❌ |
| 未知工具名 | 写回 tool_result(is_error=true) | ❌ |
| `HistoryError`(配对违反 / 投影不变量违反) | panic | ⚠️ 内部 bug |

**MaxTokens 重试后再次截断的处理**:不再升级,接受当前已截断的 assistant content,push 进 history 并继续循环——让模型在下一轮基于已截断输出继续(可能产出 "继续上面..." 的 follow-up)。

### 12.10 Session::send 的简化

```rust
impl Session {
    pub async fn send(
        &mut self,
        user_input: Vec<ContentBlock>,
        provider: &dyn Provider,
        registry: &ToolRegistry,
        compaction_policy: &dyn CompactionPolicy,
        approval_callback: ApprovalCallback,
        config: &AgentConfig,
    ) -> Result<(), Error> {
        // 从 root 派生本轮专属 cancel token,上一轮 cancel 不影响本轮
        let cancel = self.cancel_root.child_token();

        let ctx = RunContext {
            session_id: self.session_id.clone(),
            approval_mode: config.approval_mode,
            cwd: config.cwd.clone(),
            tool_timeout: config.tool_timeout,
            tool_max_output_bytes: config.tool_max_output_bytes,
            cancel,
            sink: self.sink.clone(),
        };

        run_loop(self, user_input, provider, registry,
                 compaction_policy, approval_callback, &ctx).await
    }

    /// 用户主动取消当前 send 的快捷接口
    pub fn cancel_current(&self) {
        self.cancel_root.cancel();
        // 取消后下一次 send 入口会重新 child_token,新 send 不受影响
    }
}
```

**关键变化**(对照上一轮审计 #1):
- `Session.cancel_root` 永久,不重置
- 每次 send 内部 `child_token()` 派生本轮专属 token
- `cancel_current()` 取消 root,但下一次 send 派生新 child,自动恢复
- ctrl-c handler 持有 `Arc<Session>` 调 `cancel_current` 即可,不再需要 clone token 间的同步


