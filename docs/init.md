完整方案,一次性给全。结构:协议层 → Session 持有状态 → 状态机(含三种取消场景)→ 核心循环代码 → 周边设施 → 一致性自查表。

---

# 0. 整体数据流

```text
                    ┌─────────────┐
   用户输入 ────────▶│   Session   │◀──── Ctrl-C (cancel.cancel())
                    │ (history,   │
                    │  system,    │
                    │  cancel)    │
                    └──────┬──────┘
                           │ send(input)
                           ▼
                  ┌─────────────────┐
                  │    run_loop      │ ◀── 状态机,见第3节
                  └────────┬─────────┘
                           │ emit
                           ▼
                  ┌─────────────────┐
                  │   EventSink      │── Console / Jsonl / Memory
                  └─────────────────┘
                           │
              ┌────────────┼────────────┐
              ▼            ▼            ▼
          stdout      session.jsonl   测试断言
                           │
                           ▼
                      flash replay
```

---

# 1. protocol

## 1.1 Ids

两层,不要 turn_id(分组关系靠事件顺序 + `AssistantMessageStart/End` 边界推导)。

```rust
pub struct Ids {
    pub session_id: String,
    pub call_id: String,
}
```

## 1.2 Message / Role / Prompt

```rust
pub enum Role { System, User, Assistant, Tool }

pub struct Message {
    pub role: Role,
    pub content: String,
    pub tool_call_id: Option<String>, // 仅 role=Tool 时 Some
    pub is_error: bool,               // 仅 role=Tool 时有意义;标记该 tool_result 是失败/拒绝/取消
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: Role::User, content: content.into(), tool_call_id: None, is_error: false }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: Role::Assistant, content: content.into(), tool_call_id: None, is_error: false }
    }
    pub fn tool_result(call_id: String, content: impl Into<String>, is_error: bool) -> Self {
        Self { role: Role::Tool, content: content.into(), tool_call_id: Some(call_id), is_error }
    }
}

pub struct Prompt {
    pub system: Vec<Message>,   // 全量发送,压缩不碰;只允许 role=System
    pub tools: Vec<ToolSpec>,   // 全量发送,压缩不碰
    pub messages: Vec<Message>, // 唯一可压缩部分;不允许出现 role=System
}
```

纪律靠构造函数收口:`system` 只通过 `build_system()` 追加,`messages` 只通过 `Message::user/assistant/tool_result` 构造,不裸 push 一条 `role: System` 进 `messages`。

## 1.3 ToolSpec / ToolCall

```rust
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

pub struct ToolCall {
    pub call_id: String,   // 直接复用 Provider 返回的 id
    pub name: String,
    pub input: serde_json::Value,
}
```

## 1.4 Event(完整版,含 Unknown 兼容)

```rust
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    SessionStarted { session_id: String },

    AssistantMessageStart { session_id: String },
    AssistantToken        { session_id: String, text: String },
    AssistantMessageEnd   { session_id: String },

    ToolStart     { session_id: String, call_id: String, tool: String, input: serde_json::Value },
    ToolEnd       { session_id: String, call_id: String, output: serde_json::Value, duration_ms: u64 },
    ToolError     { session_id: String, call_id: String, error: String },
    ToolCancelled { session_id: String, call_id: String },

    ApprovalRequired { session_id: String, call_id: String, command: String },
    ApprovalGranted  { session_id: String, call_id: String },
    ApprovalRejected { session_id: String, call_id: String },

    HistoryCompacted { session_id: String, before_count: usize, after_count: usize, summary: String },

    Cancelled { session_id: String, reason: String },
    Error     { session_id: String, message: String },

    #[serde(skip_serializing)]
    Unknown(serde_json::Value),
}
```

反序列化手写(方案 B,保留原始 payload,不丢数据):

```rust
impl<'de> Deserialize<'de> for Event {
    fn deserialize<D>(d: D) -> Result<Self, D::Error> where D: Deserializer<'de> {
        let value = serde_json::Value::deserialize(d)?;
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum Known { /* 与上面除 Unknown 外完全一致的 variant */ }
        match serde_json::from_value::<Known>(value.clone()) {
            Ok(k) => Ok(k.into()),
            Err(_) => Ok(Event::Unknown(value)),
        }
    }
}
```

JSONL 落盘格式固定带 `version`,版本号只是元信息,不参与兼容判断(兼容靠 `Unknown`):

```json
{"version":"1","type":"tool_start","session_id":"s1","call_id":"c1","tool":"bash","input":{"command":"ls"}}
```

---

# 2. Session(跨多轮持有状态)

`history` 不能是 `run_loop` 局部变量——函数一返回(Done 或 Cancelled)就会丢。必须提到一个跨多次用户输入存在的结构里:

```rust
pub struct Session {
    pub session_id: String,
    pub system: Vec<Message>,
    pub history: Vec<Message>,
    pub cancel: CancellationToken,
    pub sink: Arc<dyn EventSink>, // 所有函数通过 clone Arc 共享,支持并发 emit
}

impl Session {
    pub async fn send(
        &mut self,
        user_input: String,
        mode: ApprovalMode,
        provider: &dyn Provider,
        tools: &[Box<dyn Tool>],
        approval_rx: &mut mpsc::Receiver<bool>,
    ) -> Result<()> {
        self.history.push(Message::user(user_input));
        self.cancel = CancellationToken::new(); // 每轮用户输入重置,上一轮的取消不影响这一轮
        run_loop(self, mode, provider, tools, approval_rx).await
    }
}
```

`sink` 作为 `Arc<dyn EventSink>` 持有在 Session 上,而非作为参数逐层传递。这样 `execute_all_parallel` 里每个并发 future 可以 `clone()` 这个 Arc(仅引用计数+1,零拷贝代价)来安全地共享 sink。

CLI 收到 Ctrl-C 调 `session.cancel.cancel()`。用户下一次输入就是再调一次 `session.send(...)`,前提是上一轮结束时 `history` 处于合法状态(见第3节三种取消场景)。

---

# 3. 状态机(含三种取消场景)

```text
                          ┌──────┐
              用户input ─▶│ CallModel │◀────────────────────────────┐
                          └─────┬─────┘                              │
                  cancel ──────►│ (情况①:history未污染,直接Cancelled)│
                                ▼                                    │
                          ┌───────────┐                              │
                          │ Streaming │ (累积token+tool_use)          │
                          └─────┬─────┘                              │
                                ▼                                    │
                          push assistant_msg 进 history                │
                                │                                    │
                        ┌───────┴────────┐                           │
                   无tool_use       有tool_use                        │
                        │                │                           │
                        ▼                ▼                           │
                    ┌──────┐    ┌──────────────────┐                  │
                    │ Done │    │ mode==Yolo? 跳过审批 │                 │
                    └──────┘    │ mode==Default?     │                 │
                                 │ AwaitingApproval    │                 │
                                 └─────────┬───────────┘                 │
                            cancel ────────┤ (情况②:补 ToolCancelled    │
                                           │   result,Cancelled)        │
                                           ▼                            │
                         ┌─────────────────┴─────────────────┐          │
                    Rejected                              Approved      │
                         │                                    │         │
                         ▼                                    ▼         │
              补 rejected tool_result                  ExecuteTools(并发) │
              回 history,continue                      每个call独立      │
                         │                cancel(情况③:select!杀子进程,  │
                         │                       仍产出 cancelled result)│
                         │                            │                │
                         │                            ▼                │
                         │                    AppendResults 回 history  │
                         │                            │                │
                         └────────────────────────────┴────────────────┘
                                              回 CallModel
```

三种取消场景与 history 一致性:

| 场景 | 取消发生时机 | history 处理 | 能否继续对话 |
|---|---|---|---|
| ① | 模型流式输出中,assistant_msg 还没 push | 不 push,直接丢弃 | 能,history 干净 |
| ② | 等待用户审批期间,assistant_msg(带tool_use)已 push | 必须给每个 pending tool_call 补 `tool_result(is_error=true)` | 能,补完才合法 |
| ③ | 已批准,Bash 正在执行 | `select!` 杀子进程,仍产出一条 cancelled 的 tool_result | 能,天然不留洞 |

---

# 4. run_loop 完整实现

```rust
pub enum ApprovalMode { Yolo, Default }

enum ApprovalOutcome { Approved, Rejected, Cancelled }

const MAX_TURNS: usize = 50;

pub async fn run_loop(
    session: &mut Session,
    mode: ApprovalMode,
    provider: &dyn Provider,
    tools: &[Box<dyn Tool>],
    approval_rx: &mut mpsc::Receiver<bool>,
) -> Result<()> {
    let sink = session.sink.clone(); // 取出 Arc,后续传递给各子函数
    let mut turns = 0;

    loop {
        if session.cancel.is_cancelled() {
            sink.emit(Event::Cancelled { session_id: session.session_id.clone(), reason: "before model call".into() }).await;
            return Ok(());
        }
        if turns >= MAX_TURNS {
            sink.emit(Event::Error { session_id: session.session_id.clone(), message: "max turns exceeded".into() }).await;
            return Ok(());
        }
        turns += 1;

        // 压缩只动 messages,system/tools 不变
        session.history = maybe_compact(session.history.clone(), provider, &session.session_id, sink.clone()).await?;

        let prompt = Prompt {
            system: session.system.clone(),
            tools: tools.iter().map(|t| t.spec()).collect(),
            messages: session.history.clone(),
        };

        // 情况① 在这里处理:stream_model 内部 select! 监听 cancel
        let (assistant_msg, tool_calls) = match stream_model(&prompt, provider, &session.cancel, &session.session_id, sink.clone()).await {
            Ok(r) => r,
            Err(Error::Cancelled) => {
                sink.emit(Event::Cancelled { session_id: session.session_id.clone(), reason: "model streaming".into() }).await;
                return Ok(()); // history 未被污染,直接可 resume
            }
        };
        session.history.push(assistant_msg);

        if tool_calls.is_empty() {
            return Ok(()); // Done
        }

        let outcome = match mode {
            ApprovalMode::Yolo => ApprovalOutcome::Approved,
            ApprovalMode::Default => ask_approval(&tool_calls, &session.cancel, &session.session_id, approval_rx, sink.clone()).await,
        };

        match outcome {
            ApprovalOutcome::Approved => {
                let results = execute_all_parallel(&tool_calls, tools, &session.cancel, &session.session_id, sink.clone()).await;
                session.history.extend(results);
            }
            ApprovalOutcome::Rejected => {
                for call in &tool_calls {
                    sink.emit(Event::ApprovalRejected { session_id: session.session_id.clone(), call_id: call.call_id.clone() }).await;
                }
                session.history.extend(tool_calls.iter().map(|c|
                    Message::tool_result(c.call_id.clone(), "user rejected this tool call", true)
                ));
                continue; // 让模型看到拒绝原因,自己决定下一步
            }
            ApprovalOutcome::Cancelled => {
                // 情况②:补全 tool_result,保证 history 合法,再停止
                for call in &tool_calls {
                    sink.emit(Event::ToolCancelled { session_id: session.session_id.clone(), call_id: call.call_id.clone() }).await;
                }
                session.history.extend(tool_calls.iter().map(|c|
                    Message::tool_result(c.call_id.clone(), "cancelled by user before execution", true)
                ));
                sink.emit(Event::Cancelled { session_id: session.session_id.clone(), reason: "awaiting approval".into() }).await;
                return Ok(());
            }
        }
    }
}
```

`stream_model`(情况①的取消点):

```rust
async fn stream_model(
    prompt: &Prompt,
    provider: &dyn Provider,
    cancel: &CancellationToken,
    session_id: &str,
    sink: Arc<dyn EventSink>,
) -> Result<(Message, Vec<ToolCall>)> {
    sink.emit(Event::AssistantMessageStart { session_id: session_id.into() }).await;
    let mut stream = provider.stream(prompt).await;
    let mut text = String::new();
    let mut tool_calls = Vec::new();

    loop {
        tokio::select! {
            next = stream.next() => match next {
                Some(ModelEvent::Token(t)) => {
                    text.push_str(&t);
                    sink.emit(Event::AssistantToken { session_id: session_id.into(), text: t }).await;
                }
                Some(ModelEvent::ToolUse { call_id, name, input }) => {
                    tool_calls.push(ToolCall { call_id, name, input });
                }
                Some(ModelEvent::Done) | None => break,
            },
            _ = cancel.cancelled() => return Err(Error::Cancelled),
        }
    }
    sink.emit(Event::AssistantMessageEnd { session_id: session_id.into() }).await;
    Ok((Message::assistant(text), tool_calls))
}
```

`ask_approval`(三态,情况②的取消点):

```rust
async fn ask_approval(
    pending: &[ToolCall],
    cancel: &CancellationToken,
    session_id: &str,
    approval_rx: &mut mpsc::Receiver<bool>,
    sink: Arc<dyn EventSink>,
) -> ApprovalOutcome {
    for call in pending {
        sink.emit(Event::ApprovalRequired {
            session_id: session_id.into(),
            call_id: call.call_id.clone(),
            command: call.input["command"].as_str().unwrap_or_default().into(),
        }).await;
    }
    tokio::select! {
        Some(ok) = approval_rx.recv() => {
            if ok {
                for call in pending {
                    sink.emit(Event::ApprovalGranted { session_id: session_id.into(), call_id: call.call_id.clone() }).await;
                }
                ApprovalOutcome::Approved
            } else {
                ApprovalOutcome::Rejected
            }
        }
        _ = cancel.cancelled() => ApprovalOutcome::Cancelled,
    }
}
```

`execute_all_parallel`(情况③,天然不留洞;通过 `Arc` clone 实现并发安全):

```rust
async fn execute_all_parallel(
    calls: &[ToolCall],
    tools: &[Box<dyn Tool>],
    cancel: &CancellationToken,
    session_id: &str,
    sink: Arc<dyn EventSink>, // 按值传入 Arc,内部 clone 给每个 future
) -> Vec<Message> {
    let futs = calls.iter().map(|call| {
        let cancel = cancel.clone();
        let sink = sink.clone(); // 每个并发 task 拿自己的 Arc 克隆,仅引用计数+1
        let session_id = session_id.to_owned();
        let call = call.clone();
        let tool = tools.iter().find(|t| t.name() == call.name).expect("unknown tool").clone();
        async move {
            sink.emit(Event::ToolStart {
                session_id: session_id.clone(), call_id: call.call_id.clone(),
                tool: call.name.clone(), input: call.input.clone(),
            }).await;
            let started = Instant::now();
            let outcome = tool.run(call.input.clone(), cancel.clone()).await;
            let duration_ms = started.elapsed().as_millis() as u64;

            match outcome {
                ToolOutcome::Success(output) => {
                    sink.emit(Event::ToolEnd { session_id: session_id.clone(), call_id: call.call_id.clone(), output: output.clone(), duration_ms }).await;
                    Message::tool_result(call.call_id.clone(), output.to_string(), false)
                }
                ToolOutcome::Failure(err) => {
                    sink.emit(Event::ToolError { session_id: session_id.clone(), call_id: call.call_id.clone(), error: err.clone() }).await;
                    Message::tool_result(call.call_id.clone(), err, true)
                }
                ToolOutcome::Cancelled => {
                    sink.emit(Event::ToolCancelled { session_id: session_id.clone(), call_id: call.call_id.clone() }).await;
                    Message::tool_result(call.call_id.clone(), "cancelled during execution", true)
                }
            }
        }
    });
    futures::future::join_all(futs).await
}
```

并发安全说明:每个 future 持有独立的 `Arc<dyn EventSink>` clone,`emit(&self)` 内部通过 `Mutex` 保证单条写入原子性。不需要全局严格顺序——Replay/Snapshot 按 `call_id` 配对,允许事件交错(如 `ToolStart(A) ToolStart(B) ToolEnd(B) ToolEnd(A)`)。同一 call_id 的 Start→End 顺序天然保证,因为它们在同一个 future 里顺序发出。

压缩(只动 `messages`,system/tools 不变,落 `HistoryCompacted` 事件保证 Replay 一致):

```rust
const COMPACT_THRESHOLD: usize = 8000;
const KEEP_LAST_TURNS: usize = 2;

async fn maybe_compact(
    messages: Vec<Message>,
    provider: &dyn Provider,
    session_id: &str,
    sink: Arc<dyn EventSink>,
) -> Result<Vec<Message>> {
    if estimate_tokens(&messages) < COMPACT_THRESHOLD {
        return Ok(messages);
    }
    let before = messages.len();
    let split_at = messages.len().saturating_sub(KEEP_LAST_TURNS * 2);
    let (to_compact, keep) = messages.split_at(split_at);

    let summary = provider.complete_once(&Prompt {
        system: vec![Message { role: Role::System, content: COMPACT_SYSTEM_PROMPT.into(), tool_call_id: None, is_error: false }],
        tools: vec![],
        messages: vec![Message::user(serialize_for_compaction(to_compact))],
    }).await?;

    let mut out = vec![Message::user(format!("[历史摘要]\n{summary}"))];
    out.extend_from_slice(keep);

    sink.emit(Event::HistoryCompacted {
        session_id: session_id.into(), before_count: before, after_count: out.len(), summary: summary.clone(),
    }).await;
    Ok(out)
}
```

---

# 5. Provider / Tool trait

```rust
pub enum ModelEvent {
    Token(String),
    ToolUse { call_id: String, name: String, input: serde_json::Value },
    Done,
}

#[async_trait]
pub trait Provider {
    async fn stream(&self, prompt: &Prompt) -> BoxStream<'_, ModelEvent>;
    async fn complete_once(&self, prompt: &Prompt) -> Result<String>; // 仅供压缩调用,不带tools
}

pub enum ToolOutcome {
    Success(serde_json::Value),
    Failure(String),
    Cancelled,
}

#[async_trait]
pub trait Tool {
    fn name(&self) -> &str;
    fn spec(&self) -> ToolSpec;
    async fn run(&self, input: serde_json::Value, cancel: CancellationToken) -> ToolOutcome;
}

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str { "bash" }
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "Execute a shell command".into(),
            input_schema: json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}),
        }
    }
    async fn run(&self, input: serde_json::Value, cancel: CancellationToken) -> ToolOutcome {
        let cmd = input["command"].as_str().unwrap_or_default();
        let child = tokio::process::Command::new("sh").arg("-c").arg(cmd).output();
        tokio::select! {
            result = child => match result {
                Ok(out) if out.status.success() => ToolOutcome::Success(json!({"stdout": String::from_utf8_lossy(&out.stdout)})),
                Ok(out) => ToolOutcome::Failure(String::from_utf8_lossy(&out.stderr).into()),
                Err(e) => ToolOutcome::Failure(e.to_string()),
            },
            _ = cancel.cancelled() => ToolOutcome::Cancelled, // 注:用 output() 时进程kill需额外spawn管理,生产实现要保留child handle主动kill
        }
    }
}
```

(实现注记:用 `Command::output()` 拿不到 child handle 来 kill,生产代码要换成 `spawn()` + 持有 `Child` + `cancel` 分支里 `child.kill().await`,上面为了篇幅简化,逻辑结构不变。)

---

# 6. EventSink trait 与三实现(并发安全设计)

## 6.1 设计约束

并发只发生在 `execute_all_parallel`:多个 tool call 各自在独立 future 里跑,各自要调 `emit`。其余地方(`stream_model`、`ask_approval`)都是单任务顺序调用。

设计目标:**让多个并发 task 能安全地共享同一个 sink**,不需要为整条链路过度设计。

顺序保证:**不需要全局严格顺序**。Replay/Snapshot 已按 `call_id` 配对,不依赖事件相邻。唯一的硬约束是:
- 单条 JSONL 写入必须是原子的(不能两个 task 同时写导致一行被拆成两半)
- 同一个 call_id 自己的 `ToolStart→ToolEnd` 顺序不能打乱(天然满足,因为在同一个 future 里顺序发出)

## 6.2 Trait 定义

```rust
#[async_trait]
pub trait EventSink: Send + Sync {
    async fn emit(&self, event: Event);
    // 返回 () 不返回 Result:sink 自身的写入失败(比如磁盘满)
    // 不应该把 tool loop 拖垮,由各实现自己处理/记录错误。
}
```

关键变更:`&mut self` → `&self`,配合 `Send + Sync` bound,使得 `Arc<dyn EventSink>` 可以被多个并发 future 共享。

## 6.3 三个实现

**JsonlSink** —— 用 `tokio::sync::Mutex` 包 `tokio::fs::File`:

```rust
pub struct JsonlSink {
    file: tokio::sync::Mutex<tokio::fs::File>,
}

#[async_trait]
impl EventSink for JsonlSink {
    async fn emit(&self, event: Event) {
        if matches!(event, Event::Unknown(_)) { return; } // Unknown 不参与写入
        let line = match serde_json::to_string(&event) {
            Ok(l) => l,
            Err(e) => { eprintln!("serialize failed: {e}"); return; }
        };
        let mut file = self.file.lock().await;
        if let Err(e) = file.write_all(format!("{line}\n").as_bytes()).await {
            eprintln!("jsonl write failed: {e}"); // 吞掉错误,不让 sink 故障拖垮 tool loop
        }
    }
}
```

为什么不用 channel/actor:用 `Mutex<File>` + `await` 的好处是**每次 `emit().await` 返回时,这一行保证已写完**,不需要任何额外的 flush/drain/shutdown 逻辑。同进程里跑完 `flash run` 立刻做 snapshot 比较不会有竞态。只有并发量上升到锁竞争可测量影响延迟时(几十个并行 tool call,当前 v1 只有 Bash 不会发生)才值得升级成 actor。

**ConsoleSink** —— `std::sync::Mutex` 包 `Stdout`:

```rust
pub struct ConsoleSink {
    out: std::sync::Mutex<std::io::Stdout>,
}

#[async_trait]
impl EventSink for ConsoleSink {
    async fn emit(&self, event: Event) {
        let mut out = self.out.lock().unwrap();
        writeln!(out, "{}", render_human_readable(&event)).ok();
    }
}
```

**MemorySink** —— 测试用,`std::sync::Mutex<Vec<Event>>`:

```rust
pub struct MemorySink {
    events: std::sync::Mutex<Vec<Event>>,
}

#[async_trait]
impl EventSink for MemorySink {
    async fn emit(&self, event: Event) {
        self.events.lock().unwrap().push(event);
    }
}

impl MemorySink {
    pub fn snapshot(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }
}
```

## 6.4 调用方持有方式

`run_loop` 及所有子函数统一接收 `Arc<dyn EventSink>`(按值传,clone 只是引用计数+1)。Session 持有 `sink: Arc<dyn EventSink>`,在 `run_loop` 入口 clone 一次后逐层传递。

---

# 7. Replay / Snapshot / Harness

Replay:纯消费 JSONL,不调用 Provider;用 `HashMap<call_id, _>` 配对 `ToolStart`/`ToolEnd`/`ToolError`/`ToolCancelled`,不假设顺序相邻(并发执行下交错是正常的)。

Snapshot:只投影结构,跳过非确定性内容:

```rust
#[derive(PartialEq, Debug, Serialize, Deserialize)]
enum SnapshotEntry {
    ToolCall { tool: String, key_args: BTreeMap<String, Value> },
    ToolSucceeded,
    ToolFailed,
    ToolCancelled,
    AssistantTurnEnd,
}

fn to_snapshot(events: &[Event]) -> Vec<SnapshotEntry> {
    events.iter().filter_map(|e| match e {
        Event::ToolStart { tool, input, .. } => Some(SnapshotEntry::ToolCall { tool: tool.clone(), key_args: extract_whitelisted(tool, input) }),
        Event::ToolEnd { .. } => Some(SnapshotEntry::ToolSucceeded),
        Event::ToolError { .. } => Some(SnapshotEntry::ToolFailed),
        Event::ToolCancelled { .. } => Some(SnapshotEntry::ToolCancelled),
        Event::AssistantMessageEnd { .. } => Some(SnapshotEntry::AssistantTurnEnd),
        _ => None, // AssistantToken / ApprovalRequired 等不进入结构快照
    }).collect()
}
```

Harness 内容断言(单独跑,跟 snapshot 不混):

```rust
assert!(tool_called(&events, "bash"));
assert!(answer_contains(&events, "Controller"));
```

目录:`evals/cases/001/{prompt.txt, expected.json, repo/}`,`snapshots/case_001.jsonl`。

---

# 8. CLI

```bash
flash --mode yolo|default "your query here"
```

---

# 9. 明确不做(v1 红线)

```text
UI / TUI、多 Provider、MCP
Git / AST / Embedding / 向量库
细粒度权限(risk_level),只有 Yolo/Default 两态
暂停/恢复式 Cancel,单 tool 单独取消(只有整体取消)
分布式 sequence 号(单进程顺序 JSONL 够用)
多工具(只有 Bash)
分段式/加权摘要压缩(只有"老的全摘要,新的留原文"一种策略)
```

---

# 10. 一致性自查表

| 讨论决策 | 落地位置 |
|---|---|
| Tool Loop 状态机(ReAct式) | 第3节状态图 + 第4节 `run_loop` |
| 并行 tool_use 关联 ID,无 turn_id | 第1.1节 `Ids`,分组靠 `AssistantMessageStart/End` 边界 |
| serde Unknown 方案 | 第1.4节手写 `Deserialize`,`JsonlSink` 跳过写入 `Unknown` |
| Snapshot 只测结构 | 第7节 `to_snapshot`,跳过 `AssistantToken` |
| Cancel 最小设计 | 第3节三种场景表 + 第4节三处 `select!` 挂点 |
| 压缩:system/tools 不压,messages 压 | 第1.2节 `Prompt` 字段划分 + 第4节 `maybe_compact` |
| Yolo/Default 两态,只有 Bash | 第4节 `ApprovalMode` 分支 + 第5节 `BashTool` |
| 取消后能否继续对话 | 第3节场景②③ + `ApprovalOutcome::Cancelled` 分支补 `tool_result` |
| system 用 `Vec<Message>` | 第1.2节,纪律靠构造函数收口 |
| EventSink 并发安全:`&self` + `Arc` + `Mutex` | 第6节 trait 定义 + 三实现 + 第4节 `execute_all_parallel` 中 Arc clone |
| 不用 channel/actor,用 Mutex 同步等待 | 第6节设计约束:emit 返回即写完,无需 flush/shutdown |
| 事件顺序不要求全局严格 | 第6.1节:Replay 按 call_id 配对,允许交错 |

这份设计里有一处实现细节需要在写代码时补全:
`BashTool::run` 用 `Command::output()` 拿不到子进程 handle 做真正的 `kill()`,生产实现要换成 `spawn()` 持有 `Child`,在 `cancel` 分支里显式 kill,否则取消只是逻辑上停止等待,子进程实际还在跑。

关于三个 sink 的 Mutex 选型:`ConsoleSink`/`MemorySink` 用 `std::sync::Mutex`,`JsonlSink` 用 `tokio::sync::Mutex`——这是按"临界区是否跨越 `.await` 暂停点"这个标准的正确划分,不需要统一。`ConsoleSink`/`MemorySink` 的 `emit` 函数体内没有任何 `.await`,guard 在第一次 poll 时同步跑完就 drop 了,`clippy::await_holding_lock` 不会触发,不要加 `#[allow]`(否则以后真有人在里面加了 await 会被悄悄吞掉)。`JsonlSink` 临界区内有 `write_all(...).await`,guard 确实跨越暂停点,所以必须用 `tokio::sync::Mutex`——如果换成 `std::sync::Mutex`,`MutexGuard` 不是 `Send`,future 在多线程 runtime 下会编译失败。

除此之外,从状态机到事件到 Replay/Snapshot,字段命名和分支处理在这份文档里是自洽的,可以直接照着写 crate。