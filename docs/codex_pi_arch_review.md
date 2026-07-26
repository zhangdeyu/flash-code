# Codex & Pi 架构设计深度对比分析

> 研究对象：`openai/codex`（Rust + TypeScript）、`earendil-works/pi`（纯 TypeScript）
> 参照项目：`zhangdeyu/flash-code`（Rust）
> 分析维度：架构 / Runtime / Tool / 权限 / TUI

---

## 一、整体架构设计对比

### 1.1 Codex（openai/codex）整体架构

Codex 是一个高度模块化的 **Rust 单体 + TypeScript CLI** 混合架构，codex-rs 下拥有 **99 个 crate**，通过严格的 crate 边界拆分保持职责清晰：

```
┌─────────────────────────────────────────────────────────┐
│                  codex-tui (Ratatui)                     │  ← 交互层
└───────────────────────┬─────────────────────────────────┘
                        │ AppEvent / AppCommand
┌───────────────────────▼─────────────────────────────────┐
│               app-server-daemon (Daemon 进程)             │  ← IPC 总线
│  app-server-protocol (JSON + TypeScript 自动生成)         │
└──────┬─────────────────────────────────────────┬────────┘
       │ UDS / WebSocket                         │
┌──────▼────────────┐                   ┌────────▼────────┐
│   codex-core      │                   │  codex-mcp      │
│  (Agent Thread,   │                   │  (MCP Server/   │
│   Exec, Compact,  │                   │   Client)       │
│   ExecPolicy)     │                   └─────────────────┘
└──────┬────────────┘
       │
┌──────▼────────────────────────────────────────────────┐
│  sandboxing / execpolicy / linux-sandbox / bwrap       │  ← 沙箱层
│  (Seatbelt / Landlock / Docker / bwrap)                │
└───────────────────────────────────────────────────────┘
```

**核心设计亮点**：
- **严格反"上帝 crate"规则**：AGENTS.md 明确规定 **resist adding code to codex-core**，每个概念独立 crate
- **Daemon 化设计**：`app-server-daemon` 以独立进程运行，TUI 通过 UDS/WebSocket 连接，实现 GUI/Backend 完全解耦
- **自动类型生成**：Rust crate `app-server-protocol` 通过 `ts-rs` + `schemars` 自动生成 TypeScript 类型文件，保持跨语言类型安全

---

### 1.2 Pi（earendil-works/pi）整体架构

Pi 是一个轻量纯 TypeScript Monorepo，7 个子包职责边界极其清晰：

```
                    ┌─────────────────────────┐
                    │  @pi-tui                │  ← 纯 TypeScript 增量渲染 TUI
                    └────────────┬────────────┘
                                 │
┌────────────────────────────────▼────────────────────────────────┐
│              @pi-coding-agent                                    │
│  (AgentSession / Extension System / Session Tree / Compaction)  │
└────────────────┬──────────────────────────────┬─────────────────┘
                 │                              │
┌────────────────▼───────────────┐    ┌─────────▼──────────────────┐
│   @pi-agent-core               │    │ @pi-storage (SQLite WAL)    │
│ (AgentLoop / Hooks / Events)   │    └────────────────────────────┘
└────────────────┬───────────────┘
                 │
┌────────────────▼───────────────┐
│     @pi-ai                     │  ← 统一多 Provider LLM API
│ (30+ Provider / Lazy 加载)     │
└────────────────────────────────┘
```

**核心设计亮点**：
- **极简依赖哲学**：TUI 层零第三方依赖，Provider SDK 全部 Lazy 加载
- **供应链安全**：所有生产依赖 exact version 锁定，`min-release-age=2` 防止被恶意新版本劫持
- **TypeScript 纯 Erasable 模式**：禁止 `enum`/`namespace`，Node 原生 strip 支持，构建零开销

---

### 1.3 我们项目（flash-code）当前架构

```
┌─────────────────────────────────────┐
│  crates/tui (Ratatui)               │  ← 同步渲染层（单文件 1600 行）
└───────────────┬─────────────────────┘
                │ TaskRunner / RunController
┌───────────────▼─────────────────────┐
│  crates/agent (AgentRuntime)        │  ← 单文件 3517 行
└───────────────┬─────────────────────┘
                │
┌───────────────▼─────────────────────┐
│  crates/core (Protocol + Tools)     │
│  (PermissionPolicy, Event, Storage) │
└───────────────┬─────────────────────┘
                │
┌───────────────▼─────────────────────┐
│  crates/provider (ChatProvider)     │
└─────────────────────────────────────┘
```

**当前问题**：
- `agent/src/lib.rs` 3517 行，`tui/src/lib.rs` 1600 行，单文件过大
- 没有 Daemon 化设计，TUI 与 Agent 强耦合
- 无 MCP 支持，无插件/扩展机制

---

## 二、Runtime 设计对比

### 2.1 Codex Runtime — Thread Manager 多线程模型

Codex 引入了 **CodexThread** + **ThreadManager** 概念，不是简单的 `for turn in 1..=max_turns`：

```rust
// codex_thread.rs - Thread 有独立的状态机
pub struct ThreadConfigSnapshot {
    pub model: String,
    pub approval_policy: AskForApproval,      // 细粒度权限
    pub permission_profile: PermissionProfile,
    pub forked_from_thread_id: Option<ThreadId>,  // 支持 Thread Fork
    pub parent_thread_id: Option<ThreadId>,         // 支持父子 Thread
    pub reasoning_effort: Option<ReasoningEffort>,  // 推理力度控制
    pub collaboration_mode: CollaborationMode,
}
```

**亮点**：
1. **Thread 即一等公民**：每个对话是一个 `CodexThread`，有独立 ID、历史、权限配置
2. **Thread Fork/Branch**：支持从任意历史节点 Fork 新 Thread（类 Git 分支）
3. **Mid-turn Compaction**：在 LLM 调用中途检测 context overflow，触发压缩不中断当前 turn
4. **World State 概念**：有 `WorldState` 全局状态，用于 Context 注入控制

### 2.2 Pi Runtime — Hooks 钩子驱动模型

Pi 的 `AgentLoopConfig` 通过回调函数暴露完整生命周期钩子：

```typescript
interface AgentLoopConfig {
  convertToLlm: (messages: AgentMessage[]) => Message[]  // 格式转换
  transformContext?: (messages, signal) => Promise<AgentMessage[]>  // 上下文变换
  getApiKey?: (provider) => Promise<string>              // 动态 API Key
  shouldStopAfterTurn?: (ctx) => boolean                 // 提前停止
  prepareNextTurn?: (ctx) => AgentLoopTurnUpdate         // Turn 前注入
  getSteeringMessages?: () => Promise<AgentMessage[]>    // 中途转向
  getFollowUpMessages?: () => Promise<AgentMessage[]>    // 后置跟进
  beforeToolCall?: (ctx, signal) => BeforeToolCallResult // 工具前拦截
  afterToolCall?: (ctx, signal) => AfterToolCallResult   // 工具后覆写
  toolExecution?: "sequential" | "parallel"              // 并行执行模式
}
```

**亮点**：
1. **工具并行执行**：`parallel` 模式下，多个工具调用并发执行，按完成顺序 emit 事件
2. **Steering + Follow-up 双队列**：用户可在 Agent 运行时中途插入转向指令或追加任务
3. **TypeScript Declaration Merging 扩展消息类型**：通过 `interface CustomAgentMessages` 扩展，无需修改核心库

### 2.3 我们项目 Runtime

```rust
// 简洁但灵活性不足
pub async fn run_with_start_and_controls(...) {
    for turn in 1..=self.options.max_turns {  // 简单的 for 循环
        // check cancel → send request → handle events → handle tools
    }
}
```

**问题**：
- 纯 for 循环，无 Thread 概念，无 Fork
- 没有 mid-turn compaction
- 无 beforeToolCall/afterToolCall 钩子
- 并发工具调用未支持

---

## 三、Tool 系统设计对比

### 3.1 Codex Tool 系统

Codex 有精细的 **Unified Exec** 架构，shell 命令通过专用模块处理：

```
工具调用 → exec_policy.rs (策略检查) → ExecApprovalRequirement
                                              │
                          ┌───────────────────┼──────────────────────┐
                     Allow (自动执行)      Ask (请求审批)        Deny (直接拒绝)
                          │                   │
                    exec.rs                  TUI 显示 ApprovalRequest
                          │
                    sandboxing::SandboxManager
                          │
           ┌──────────────┼──────────────┐
      Seatbelt          Landlock       Windows Restricted Token
     (macOS)          (Linux)           (Windows)
```

**亮点**：
1. **BANNED_PREFIX_SUGGESTIONS**：内置禁止前缀列表（`bash -c`, `node -e`, `python -c` 等），防止 shell 注入绕过
2. **平台原生沙箱**：macOS 用 Seatbelt，Linux 用 Landlock+bwrap，Windows 用 Restricted Token，不是简单的 process sandbox
3. **网络策略分离**：NetworkRule 独立配置，`http://` 和 `https://` 分别管理

### 3.2 Pi Tool 系统

Pi 工具系统通过 `beforeToolCall`/`afterToolCall` 两个 Hook 实现权限控制，工具定义采用 **TypeBox** JSON Schema：

```typescript
// 工具定义使用类型安全的 JSON Schema
interface AgentTool<T extends TSchema> {
  name: string
  description: string
  schema: T   // TypeBox Schema
  execute: (args: Static<T>, context, signal) => Promise<ToolResult>
}
```

**亮点**：
1. **TypeBox 强类型 Schema**：工具参数有编译时类型检查，不是运行时字符串验证
2. **工具结果可覆写**：`afterToolCall` 可以完全替换工具返回值，方便日志注入/结果过滤

### 3.3 我们项目 Tool 系统

```rust
pub trait Tool {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;  // 纯 JSON，无类型安全
    fn risk(&self, input: &Value) -> Result<ToolRisk, ToolError>;
    fn call(&self, input: Value, context: &ToolContext) -> Result<ToolOutput, ToolError>;
}
```

**问题**：
- 工具定义是同步的，无法支持流式输出
- 无 beforeCall/afterCall 钩子
- 缺乏工具并发执行能力

---

## 四、权限/沙箱系统对比

### 4.1 Codex — 多层策略引擎

```
用户请求执行命令
        │
        ▼
┌─────────────────────────────────────────────┐
│  ExecPolicyEngine                           │
│  Policy { rules_by_program, network_rules } │
│                                             │
│  Step 1: is_known_safe_command?             │
│  Step 2: is_dangerous_command_match?        │
│  Step 3: prefix rule matching               │
│  Step 4: heuristics fallback                │
└─────────────────────────┬───────────────────┘
                          │
              Decision: Allow / Ask / Deny
                          │
            ┌─────────────┼─────────────┐
          Allow          Ask           Deny
            │             │
         直接执行     显示 ApprovalRequest UI
                     用户可选择：Allow-Once / Allow-Always / Deny
                                  │
                             Allow-Always → 追加 rule 到 policy 文件
```

**亮点**：
1. **策略文件持久化**：用户批准后写入 `.rules` 文件，下次自动放行
2. **Program-indexed MultiMap**：以命令名为 key 的 MultiMap，O(1) 查找效率
3. **带 Justification 的规则**：每条规则可附带 `justification` 字段，解释为何允许/拒绝
4. **网络策略分离**：网络访问有独立的 `NetworkRule` 系统

### 4.2 Pi — Project Trust + 理性安全边界

Pi 采用了非常务实的安全哲学：
- **不提供进程内伪沙箱**（会给用户虚假安全感）
- **Project Trust 机制**：检测 `.pi/settings.json` 等本地配置，首次使用要求用户授权
- **推荐外部沙箱**：Gondolin（micro-VM）/ Docker / NVIDIA OpenShell

**亮点**：
- 诚实告知用户安全边界，不用"沙箱"欺骗用户
- Project Trust 存储在 `~/.pi/agent/trust.json`，支持父目录继承

### 4.3 我们项目权限系统

```rust
pub enum ToolRisk { Read, Write, Execute, Network, Destructive }

pub const fn decide(self, risk: ToolRisk) -> PermissionDecision {
    match (self.mode, risk) {
        (ApprovalMode::Human, _) => PermissionDecision::Deny,
        (_, ToolRisk::Destructive) => PermissionDecision::Ask,
        (ApprovalMode::Confirm, ToolRisk::Read) => PermissionDecision::Allow,
        (ApprovalMode::Yolo, ...) => ...
    }
}
```

**现状**：
- 权限系统简单但有效（3 个 ApprovalMode + 5 个 ToolRisk）
- 缺少命令前缀规则引擎（Codex 的 PrefixRule 系统）
- 缺少持久化的用户批准记忆（每次都要重新审批）
- 没有网络访问策略

---

## 五、TUI 设计对比

### 5.1 Codex TUI — 高度模块化事件驱动

Codex TUI 有 **130+ 源文件**，采用类似 Elm Architecture 的事件驱动模型：

```rust
// 核心事件总线
pub enum AppEvent {
    // 300+ 事件变体
    ApprovalRequested(ApprovalRequest),
    ThreadGoalSet { mode, goal_draft },
    PluginInstalled(PluginInstallResponse),
    RateLimitRefresh(RateLimitRefreshOrigin),
    ...
}

// AppEventSender: 跨组件发送事件，组件不直接访问 App
pub struct AppEventSender { tx: Sender<AppEvent> }
```

**亮点**：
1. **AppEvent 作为统一内部总线**：Widget 通过 `AppEventSender` 发送事件，避免组件间直接耦合
2. **Markdown 流式渲染**：`markdown_stream.rs` + `markdown_render.rs`，支持流式 Markdown 渲染，不等待完整输出
3. **Diff 可视化**：`diff_render.rs` 专门的 patch diff 渲染模块
4. **多 Agent 并行显示**：`multi_agents.rs` 支持显示多个 Agent 并行工作状态
5. **Terminal 超链接**：`terminal_hyperlinks.rs` 支持终端内嵌可点击链接
6. **Pager 覆盖层**：`pager_overlay.rs` 类似 less 的内嵌 pager
7. **外部编辑器支持**：`external_editor.rs` 支持调用 $EDITOR 编辑输入

### 5.2 Pi TUI — 零依赖增量渲染

```typescript
// Component 接口 - 极简设计
export interface Component {
  render(width: number): string[]  // 渲染为 ANSI 字符串行
  handleInput?(data: string): void // 键盘输入
  wantsKeyRelease?: boolean        // Kitty 协议支持
  invalidate(): void               // 缓存失效
}

// 增量渲染 - 只更新变化的行
function renderDiff(previousLines: string[], newLines: string[]): void {
  // 对比每行，只输出 ANSI 控制序列更新变化部分
}
```

**亮点**：
1. **零第三方依赖**：不依赖 Ratatui/Ink/Blessed，完全自主实现
2. **CURSOR_MARKER 技术**：通过 APC 序列 `\x1b_pi:c\x07` 标记光标位置，支持 IME 输入法
3. **Kitty Image Protocol**：原生支持终端内图片显示
4. **增量渲染算法**：只对改变的行列输出 ANSI 指令，无闪烁

### 5.3 我们项目 TUI

```rust
// 单文件 1600 行，同步渲染
fn render_frame(stdout: &mut impl Write, state: &AppState) -> Result<(), TuiError> {
    // 每帧全量重绘
}

// input_loop: 事件处理
async fn input_loop(root, stdout, state, runner) {
    // 键盘事件 → 直接修改 state → 重绘
}
```

**现状**：
- 功能基本可用，但架构简单
- 无事件总线，state 直接修改
- 无 Markdown 渲染
- 无 Diff 可视化
- 无流式渲染支持

---

## 六、可以学习的核心经验汇总

### 🔴 高优先级（架构级改进）

#### 1. Agent Runtime 模块拆分
- **问题**：`agent/src/lib.rs` 3517 行，`tui/src/lib.rs` 1600 行
- **Codex 做法**：AGENTS.md 明文规定 ≤500 行/模块，超过 800 行必须拆分
- **Pi 做法**：`agent-loop.ts`, `agent.ts`, `types.ts` 各司其职
- **建议**：将 `agent/src/lib.rs` 拆分为 `session.rs`, `turn.rs`, `tool_dispatch.rs`, `retry.rs` 等子模块

#### 2. 工具生命周期钩子（beforeToolCall / afterToolCall）
- **Pi 设计**：beforeToolCall 可阻断工具执行；afterToolCall 可覆写结果
- **价值**：支持权限扩展插件、结果日志注入、审计等，无需修改核心代码
- **建议**：在 `ToolRegistry` 中增加可选的 pre/post 拦截器

#### 3. 会话树与分支（Thread Fork）
- **Codex/Pi 均有**：支持从历史任意节点 Fork 新会话
- **Pi 实现**：JSONL 格式，每条记录含 `parentId`，支持树状结构
- **建议**：将 session storage 升级为支持父子关系的树状存储，而非纯线性

#### 4. 上下文压缩（Compaction）
- **Codex**：mid-turn 和 pre-turn 两种压缩模式，不中断当前 Turn
- **Pi**：计算 token 数，保留首尾关键消息，压缩中间历史
- **建议**：实现自动 compaction，当 prompt 接近 max_prompt_bytes 时触发

---

### 🟡 中优先级（功能完善）

#### 5. 工具并行执行
- **Pi 设计**：`toolExecution: "parallel"` 模式，多工具并发执行
- **价值**：对 Read+Grep+List 等只读工具可显著提速
- **建议**：在 `handle_provider_events` 中检测独立工具调用，并发执行

#### 6. 权限规则持久化
- **Codex 做法**：用户批准后写入 `.rules` 文件，支持 `Allow-Always`
- **建议**：增加 `~/.flash/approved_commands.rules` 文件，避免重复审批

#### 7. TUI 事件总线（AppEvent 模式）
- **Codex 做法**：`AppEvent` enum + `AppEventSender`，组件不直接修改全局 state
- **建议**：将 TUI 重构为事件驱动，`AppState` 只能通过 `AppEvent` 修改

#### 8. Steering Messages（中途转向）
- **Pi 设计**：`getSteeringMessages()` 允许用户在 Agent 工作中途插入指令
- **建议**：在 TUI 的 `input_loop` 中，用户输入的新指令放入 steering queue，而非强制取消当前任务

---

### 🟢 低优先级（质量提升）

#### 9. 供应链安全实践
- **Pi 做法**：`exact version` + `min-release-age=2` + npm OIDC publishing
- **建议**：在 `Cargo.toml` 中明确版本，考虑使用 `cargo-deny` 检查依赖

#### 10. Banned Prefix 列表
- **Codex 做法**：硬编码 `BANNED_PREFIX_SUGGESTIONS`，防止 `bash -c` 等 shell 注入
- **建议**：在 `BashTool` 中增加危险命令检测，高风险命令强制 Ask 审批

#### 11. Markdown 流式渲染
- **Codex 做法**：流式渲染 Markdown，不等待完整输出再显示
- **建议**：TUI 中的 assistant delta 支持增量 Markdown 渲染

#### 12. Context Limit 截断防御
- **Pi 做法**：检测 `stopReason === "length"`，自动将截断的工具调用置为失败
- **建议**：在 `handle_provider_events` 中检测 length stop，防止执行损坏的工具调用

---

## 七、与我们项目的直接对比总结

| 维度 | Codex | Pi | flash-code | 差距/机会 |
|------|-------|-------|------------|----------|
| **架构模块化** | ★★★★★ 99 crates | ★★★★★ 7 packages | ★★★☆☆ 8 crates | 需要继续拆分大文件 |
| **Runtime 钩子** | ★★★★☆ Hook Runtime | ★★★★★ 完整 Hooks | ★★☆☆☆ 基本事件 | 增加 before/after hook |
| **工具并发** | ★★★☆☆ 部分支持 | ★★★★★ 并行模式 | ★★☆☆☆ 串行 | 实现并行工具执行 |
| **权限系统** | ★★★★★ Policy Engine | ★★★☆☆ Trust 系统 | ★★★☆☆ 简单分级 | 增加规则持久化 |
| **Context 管理** | ★★★★★ Compaction | ★★★★★ 自动压缩 | ★★★☆☆ 简单截断 | 实现 Compaction |
| **TUI 功能** | ★★★★★ 130+ 文件 | ★★★★☆ 零依赖精良 | ★★★☆☆ 基础可用 | 事件总线 + Markdown |
| **Session 管理** | ★★★★★ Thread Fork | ★★★★★ 树状 Session | ★★★☆☆ 线性存储 | 树状 Session |
| **MCP 支持** | ★★★★★ 完整 | ★★☆☆☆ 无 | ★☆☆☆☆ 无 | 高价值特性 |
| **Provider 支持** | ★★★★★ 多 Provider | ★★★★★ 30+ Provider | ★★★☆☆ 少数 | 扩展 Provider |

---

*报告生成时间：2026-07-26*
