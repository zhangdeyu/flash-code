# Flash Code 渐进式迭代计划

本文档定义 Flash Code 从空项目到 1.0 可用版本的迭代计划。计划遵循三个原则:

- 先稳定协议、存储、runtime,再做 TUI 体验。
- 所有 CLI、TUI、Eval 都调用同一个 Agent runtime。
- 每个迭代都必须有可运行交付和可验证验收标准。

## 1. 版本总览

| 版本 | 主题 | 核心结果 |
|---|---|---|
| 0.1 | 协议、存储、CLI 骨架 | 项目可启动,session 可落盘和 replay |
| 0.2 | DeepSeek Headless Agent MVP | `flash run` 能完成最小模型-工具闭环 |
| 0.3 | 代码修改能力 | Agent 能读、改、测一个真实 workspace |
| 0.3.x | 工具协议收敛 | Agent 可见工具稳定为 `Read/Edit/Write/Glob/Grep/ListFiles/Bash` |
| 0.4 | TUI MVP | `flash` 默认进入 TUI,并复用同一 runtime |
| 0.5 | 内部自动化测试体系 | 核心协议、存储、工具、TUI 有防退化测试 |
| 0.6 | Terminal-Bench Smoke | 接入主公开基准的小样本评测 |
| 0.6.x | Skill 机制评估点 | 仅在评测或真实工作流证明需要时引入 |
| 0.7 | SWE-bench Verified Smoke | 接入真实 issue 修复类评测 |
| 0.7.x | SubAgent / Task / AskUser 评估点 | 仅在单 Agent loop 和 TUI 状态模型不足时引入 |
| 0.8 | 回归评测与报告 | 形成 release 前防退化机制 |
| 1.0 | 日常可用版本 | TUI、CLI、session、评测路径完整可用 |

## 1.1 当前状态

已完成并推送:

- 0.1 协议、存储、CLI 骨架。
- 0.2 Headless Agent loop。
- 0.3 代码修改能力。
- 0.3.x 工具协议收敛。
- 0.4.1 TUI Shell 与事件渲染。
- 0.4.2 TUI Session 与输入流。
- 0.4.3 TUI Approval 与 Resume。
- 0.5.1 协议与存储测试加固。
- 0.5.2 Tool 与 Permission 测试加固。

下一阶段从 0.5.3 Agent 与 TUI 测试加固开始。0.4 之后不重新设计 runtime,只把 TUI 作为 `Event` 的消费者和 `UserAction` 的生产者接入现有 Agent runtime。

## 1.2 执行节奏

每个迭代按同一节奏执行:

1. 更新或确认本阶段设计边界。
2. 实现最小可运行功能。
3. 补齐本阶段测试和验收记录。
4. 跑全局验收门槛。
5. 更新文档。
6. commit 并 push 到 `origin/feature/init`。

每个迭代必须产出 `docs/acceptance_<version>.md`,记录实际执行的命令、关键输出和验收标准映射。

## 2. 全局验收门槛

每个迭代都必须满足:

- `cargo fmt --check` 通过。
- `cargo clippy --all-targets --all-features -- -D warnings` 通过。
- `cargo test --all-features` 通过。
- 新增用户可见行为必须有文档。
- 新增 session/event/message 格式必须能 replay 或有 golden test。
- 不引入与当前迭代目标无关的大型抽象。

## 2.1 Agent 可见工具协议

v1 暴露给 Agent 的工具保持少而稳定。公开给模型的工具名与 DeepSeek 无关,provider adapter 只负责把通用 tool spec 转成 DeepSeek tool calls 格式。

| 工具 | 作用 | 默认风险 |
|---|---|---|
| `Read` | 读取指定文件的完整或部分内容 | Read |
| `Edit` | 对现有文件进行精准、有针对性的修改 | Write |
| `Write` | 创建或完全覆写文件内容 | Write |
| `Glob` | 基于模式匹配快速查找文件和目录路径 | Read |
| `Grep` | 在文件内容中通过正则或关键字搜索代码逻辑 | Read |
| `ListFiles` | 列出特定路径下的文件和目录结构 | Read |
| `Bash` | 在隔离或本地环境中执行 shell 命令行操作 | Execute / Destructive |

实现层可以继续拆小工具,但 provider tool spec 和 prompt 中优先使用上面的高层工具名。当前旧实现映射:

| 当前实现 | 目标 Agent 工具 |
|---|---|
| `read_file` | `Read` |
| `apply_patch` | `Edit` |
| `write_file` | `Write` |
| `search` | 拆分为 `Glob` / `Grep` / `ListFiles` |
| `shell` | `Bash` |
| `run_tests` | `Bash` 的受控场景或内部 helper |
| `git_diff` | `Bash` / `Read` 的受控场景或内部 helper |

后置工具不进入 v1 默认暴露:

| 工具 | 后置原因 |
|---|---|
| `TaskCreate` / `TaskUpdate` / `TaskGet` / `TaskList` | 需要任务状态模型、TUI 展示和 resume 语义 |
| `Agent` | 属于 SubAgent,会引入独立上下文、权限继承和事件归属 |
| `AskUserQuestion` | 需要同时定义 TUI、CLI、headless eval 下的交互行为 |

后置工具的评估原则:

- 先用 `events.jsonl` 和 TUI 本地状态表达进度,不足时再引入 `Task*`。
- 先用单 Agent loop 完成 Terminal-Bench/SWE-bench smoke,不足时再引入 `Agent`。
- 先用 approval 和失败状态处理歧义,确实需要模型主动询问用户时再引入 `AskUserQuestion`。
- 后置工具一旦引入,也必须保持 provider-agnostic,不能绑定 DeepSeek。

允许暂缓:

- 非 v1 必需的 provider。
- Web UI。
- parallel tool calls。
- 动态 Skill 系统。
- SubAgent 调度和 `Agent` 工具。
- `Task*` 工具。
- `AskUserQuestion` 工具。
- 独立 telemetry/trajectory/compaction 系统。

## 3. 0.1 协议、存储与 CLI 骨架

目标:项目能启动,核心协议能落盘,session 能创建、resume 校验和 replay。

范围:

- 初始化 Rust workspace。
- 定义最小 core 协议。
- 定义 provider/tool trait。
- 实现本地 workspace/session 存储。
- 实现最小 CLI 命令。
- 实现配置读取和 doctor。

交付:

- Cargo workspace。
- crates:
  - `core`
  - `provider`
  - `cli`
- `flash --version`。
- `flash init`。
- `flash doctor`。
- `flash replay <events.jsonl>`。
- `flash resume <session_id>` 的 workspace 校验。
- `~/.config/flash-code/config.toml` 用户级配置读取。
- `<workspace-root>/.flash/config.toml` workspace 级配置读取。
- `.flash/workspace.json`。
- `.flash/sessions/session_xxx/session.json`。
- `.flash/sessions/session_xxx/messages.jsonl`。
- `.flash/sessions/session_xxx/events.jsonl`。
- `Message` / `Event` 基础类型。
- `Provider` trait。
- `Tool` trait。
- `ToolRegistry` 雏形。
- `PermissionPolicy` 类型定义。

验收标准:

- 在 git repo 内执行 `flash doctor` 能识别 git root 为 workspace root。
- 在非 git repo 内执行 `flash doctor` 能识别当前 canonical cwd 为 workspace root。
- `.flash/` 默认被 `.gitignore` 忽略。
- 配置优先级符合 `default -> user config -> workspace config -> env -> CLI args`。
- `DEEPSEEK_API_KEY` 缺失时 `flash doctor` 明确提示,但不打印 secret。
- `flash init` 能创建 `.flash/workspace.json` 和 `.flash/sessions/`。
- 首次 `flash run` 或 `flash doctor` 可以按需初始化 `.flash/`。
- 新建 session 后生成 `session.json`。
- user message 能 append 到 `messages.jsonl`。
- event 能 append 到 `events.jsonl`。
- `flash replay .flash/sessions/session_xxx/events.jsonl` 能打印事件时间线。
- 在 B 目录执行 `flash resume` A 目录的 session 会拒绝并提示 workspace 不匹配。
- 同名 tool 注册失败并返回明确错误。
- permission policy 能对 Read/Write/Execute/Network/Destructive 输出 allow/ask/deny。

退出条件:

- 没有真实模型调用也能通过 mock session 验证存储、replay、resume 边界。
- 0.2 可以直接在此基础上接入 DeepSeek provider。

## 4. 0.2 DeepSeek Headless Agent MVP

目标:没有 TUI 也能完成最小 Agent loop,用于自动化、CI 和后续 benchmark。

范围:

- 接入 DeepSeek chat completion。
- 支持 streaming。
- 支持 reasoning/text/tool call/usage 事件。
- 实现顺序 Agent loop。
- 实现只读和命令类基础工具,并开始向 Agent 可见工具协议收敛。

交付:

- `deepseek` crate。
- `agent` crate。
- DeepSeek SSE parser。
- DeepSeek error mapper。
- `flash run "<task>"`。
- reasoning/text 流式输出。
- Agent 可见工具雏形:`Read`、`ListFiles`、`Bash`。
- 内部兼容实现:`read_file`、`search`、`shell`。
- `confirm` / `yolo` / `human` 三种审批模式的最小实现。
- 工具 stdout/stderr 进入 `events.jsonl`。
- 长工具输出写入 `artifacts/`。
- 顺序 loop:

```text
model -> tool approval -> tool execution -> tool result -> model
```

验收标准:

- mock DeepSeek streaming 测试覆盖 reasoning delta、text delta、tool call、usage、done。
- DeepSeek adapter 能把原始 SSE 转换为 provider-agnostic `ProviderEvent`。
- `flash run "list files"` 能触发 `ListFiles` 或兼容 search tool 并正常结束。
- tool call 成功、失败、拒绝、取消都会产生 tool result。
- unknown tool 不 panic,写 error tool result。
- 每轮 loop 都受 `max_turns` 限制,超限后 session 明确失败并写 event。
- v1 不执行 parallel tool calls;多个 tool call 按稳定顺序执行并写 event。
- 模型流取消后不把半截 assistant message 写入 `messages.jsonl`。
- shell timeout 后能终止进程并写入 error tool result。
- `events.jsonl` 能 replay 出模型输出、reasoning 和工具输出。
- 429/500/503 有有限重试。
- 401/402 不重试,直接给出可诊断错误。
- DeepSeek API key 只从环境变量读取,不会写入日志、messages 或 events。

退出条件:

- `flash run` 可以在真实 workspace 完成简单只读任务。
- 失败任务可通过 replay 复盘到模型输出和工具执行过程。

## 5. 0.3 代码修改能力

目标:Agent 能在真实 workspace 中完成小型代码修改并运行验证命令。

范围:

- 增加 `Edit` / `Write` 能力。
- 增加 git diff 和测试执行能力。
- 强化 workspace 写入边界和 destructive 审批。
- 加入简单 prompt projection/token budget。

交付:

- Agent 可见工具:`Read`、`Edit`、`Write`、`Glob`、`Grep`、`ListFiles`、`Bash`。
- 内部兼容实现:`apply_patch`、`write_file`、`git_diff`、`run_tests`。
- workspace path guard。
- command risk classifier。
- 简单 prompt projection。
- 简单 token budget 策略。
- 上下文超限处理:先报错或保留最近消息,不做复杂 compaction。

验收标准:

- `flash run "fix failing tests"` 能执行读文件、改文件、跑测试的闭环。
- 所有文件写入限制在 workspace root 内。
- 路径穿越、symlink 越界、绝对路径越界都会被拒绝并写 event。
- destructive 命令即使在 `yolo` 下也必须确认。
- `human` 模式下不会真实执行 shell/write/test,只输出建议操作。
- 修改后输出最终 diff 摘要。
- 测试失败时保留关键 stderr/stdout 摘要。
- 测试输出过长时写入 artifact,message/event 只保留摘要和路径。
- apply patch 失败时保留失败原因,不会留下半提交 message。
- 同一任务失败时可通过 replay 看到关键工具步骤。

退出条件:

- 在一个小型 Rust fixture 项目中,Agent 能修复一个已知失败测试。
- 代码修改路径不需要 TUI 也能被 benchmark adapter 复用。

## 5.1 0.3.x 工具协议收敛

目标:把 0.2/0.3 的内部工具名收敛为稳定 Agent 可见工具协议。

交付:

- provider tool spec 暴露 `Read`、`Edit`、`Write`、`Glob`、`Grep`、`ListFiles`、`Bash`。
- runtime 兼容旧工具名一段时间,避免已保存 session replay 失效。
- `Glob` 支持路径 pattern 查找。
- `Grep` 支持关键字或正则内容搜索。
- `ListFiles` 支持列出目录结构。
- `Bash` 统一承载 `run_tests` 和 `git_diff` 这类命令型场景。

验收标准:

- `flash run "list files"` 触发 `ListFiles`。
- `flash run "fix failing tests"` replay 中出现 `Read`、`Edit`、`Bash` 或兼容别名映射事件。
- 旧工具名 `read_file`、`apply_patch`、`write_file`、`search`、`shell` 仍能被 runtime 解析。
- `Glob`、`Grep`、`ListFiles` 都是 Read 风险。
- `Edit`、`Write` 都受 workspace path guard 保护。
- `Bash` 对 destructive 命令仍返回 Destructive 风险。
- 验收通过后提交并 push。

## 6. 0.4 TUI MVP

目标:TUI 成为主交互界面,无参数 `flash` 默认进入 TUI,但不改变 runtime。

范围:

- 引入 TUI crate。
- TUI 只发 `UserAction`,只消费 `Event`。
- 实现 transcript、reasoning、tool output、approval、input、cancel、resume。
- 复用 0.2/0.3 的 Agent runtime。

交付:

- `tui` crate。
- 无参数 `flash` 进入 TUI。
- `flash tui` 作为显式别名。
- 当前 workspace session 列表。
- transcript 视图。
- reasoning 展开/折叠。
- tool output 视图。
- approval UI。
- input box。
- cancel。
- resume 当前 workspace session。
- error 状态展示。

验收标准:

- 无参数执行 `flash` 会进入 TUI,不会进入 headless run。
- `flash tui` 与 `flash` 进入同一个 TUI。
- 带子命令时进入 CLI 模式,例如 `flash run`、`flash replay`、`flash doctor`。
- TUI 能列出当前 workspace 的 sessions,列表通过扫描 `session.json` 得到,不依赖 `index.json`。
- TUI 能从 `events.jsonl` 重建 transcript。
- TUI 执行任务和 `flash run` 使用同一个 Agent runtime。
- TUI 中审批结果写入 `approval_resolved` event。
- TUI 取消正在 streaming 的模型后,不写半截 assistant message。
- TUI 退出后 terminal raw mode/alternate screen 正常恢复。
- 窄屏下主要文本不重叠。
- TUI snapshot 覆盖普通输出、reasoning、tool output、approval、error。

退出条件:

- 用户可以只用 `flash` 完成一次简单代码修改任务。
- TUI 没有引入独立业务逻辑分支。

## 6.1 0.4.1 TUI Shell 与事件渲染

目标:先把 TUI 外壳和事件渲染做稳,不急着承载完整交互。

范围:

- 建立 `tui` crate。
- 接入 ratatui/crossterm 或同等 Rust TUI 基础栈。
- 建立 TUI app state。
- 从已有 `events.jsonl` 渲染 transcript。
- 支持退出、滚动、reasoning 展开/折叠。

交付:

- `flash` 无参数进入 TUI。
- `flash tui` 进入同一个 TUI。
- TUI 可以打开当前 workspace 的最近 session。
- transcript 区域能展示 user、assistant、tool、error。
- reasoning 可折叠展示。
- tool output 可折叠展示。
- error 状态有独立样式。

验收标准:

- `flash --help`、`flash run`、`flash replay`、`flash doctor` 仍保持 CLI 行为。
- 无参数 `flash` 和 `flash tui` 都进入 alternate screen。
- 退出后 terminal raw mode 和 cursor 状态恢复。
- 使用 0.3.x 产生的 `events.jsonl` 可以重建 transcript。
- snapshot 覆盖空状态、普通输出、reasoning、tool output、error。
- 窄宽度终端下文本不互相覆盖。
- 本阶段不直接调用 Provider 或 Tool。

退出条件:

- TUI 可以稳定作为 replay viewer 使用。
- TUI shell 不影响 headless `flash run`。

## 6.2 0.4.2 TUI Session 与输入流

目标:TUI 可以创建任务、输入消息、展示 live event,但仍复用同一个 Agent runtime。

范围:

- TUI input box。
- TUI 内创建新 session。
- TUI 订阅 Agent runtime 事件流。
- 展示 streaming text/reasoning。
- 展示 tool call lifecycle。
- 支持 cancel。

交付:

- 新建任务输入框。
- 当前 session 状态栏。
- live transcript。
- tool call 状态:requested、started、completed、failed、rejected。
- cancel 按键。
- 运行中、成功、失败、取消四种状态。

验收标准:

- TUI 发起任务和 `flash run` 使用同一 `AgentRuntime` 入口。
- streaming text/reasoning 先写 event,再由 TUI 渲染。
- cancel 后 session 状态为 canceled 或 failed,不会写入半截 assistant message。
- tool result 在 TUI 和 replay 中展示一致。
- TUI crash 或主动退出后,已写入 events 仍可 replay。
- headless eval 不依赖 TUI 模块。

退出条件:

- 用户可以在 TUI 中完成一个只读任务。
- 运行中取消不会破坏 session 文件。

## 6.3 0.4.3 TUI Approval 与 Resume

目标:TUI 承载真实代码修改所需的人机确认和 session 恢复。

范围:

- approval modal 或 inline panel。
- approve/reject 输入。
- destructive 命令强制确认。
- workspace session 列表。
- resume 同 workspace session。
- 跨 workspace resume 拒绝。

交付:

- approval UI。
- session list。
- session detail/replay。
- resume action。
- workspace mismatch 错误展示。
- 权限模式展示:confirm/yolo/human。

验收标准:

- `Edit`、`Write`、`Bash` 需要确认时,TUI 能渲染待审批项。
- 审批结果写入 `approval_resolved` event。
- destructive 命令在任何模式下都必须显式确认。
- `human` 模式下 TUI 只展示建议操作,不执行写入或命令。
- TUI session 列表通过扫描 `.flash/sessions/*/session.json` 生成,不依赖 `index.json`。
- 在 B workspace resume A workspace session 会拒绝并展示明确错误。
- TUI 中完成一次小型 Rust fixture 修复,最终测试通过且 diff 可见。

退出条件:

- 用户可以只用 `flash` 完成一次简单代码修改任务。
- TUI 与 CLI 在权限、存储、replay 上行为一致。

## 7. 0.5 内部自动化测试体系

目标:核心功能具备稳定防退化能力,先把内部质量体系打牢。

范围:

- 单元测试覆盖核心协议。
- 集成测试覆盖 session/tool/provider/agent loop。
- golden test 覆盖 event replay。
- mock server 覆盖 DeepSeek 行为。
- TUI snapshot 覆盖关键状态。
- CI 一键运行。

交付:

- unit tests。
- integration tests。
- mock DeepSeek server。
- golden event log tests。
- basic TUI snapshot tests。
- CI 脚本。
- fixture workspace。
- smoke command 脚本。

验收标准:

- `cargo test --all-features` 通过。
- `cargo nextest run --all-features` 通过。
- Provider 错误映射覆盖 400/401/402/422/429/500/503。
- History 测试覆盖 tool use/result 配对。
- Event replay golden 测试能捕捉事件顺序变化。
- storage 测试覆盖 workspace resume 拒绝。
- storage 测试覆盖 interrupted turn 不进 `messages.jsonl`。
- permission 测试覆盖 confirm/yolo/human。
- path guard 测试覆盖 workspace 内、workspace 外、symlink 越界。
- TUI snapshot 覆盖 transcript、reasoning、tool output、approval、error。
- CI 中不会要求真实 DeepSeek API key。

退出条件:

- 每次改协议、存储、tool loop 都能被自动化测试捕捉主要退化。
- 可以放心接入公开 benchmark smoke。

## 7.1 0.5.1 协议与存储测试加固

目标:把最容易影响 replay/resume 的稳定协议先锁住。

交付:

- event schema golden tests。
- message schema golden tests。
- session storage integration tests。
- config precedence tests。
- workspace root detection tests。

验收标准:

- 新增 event 字段必须兼容旧 golden 或显式更新 golden。
- session scan 不依赖 `index.json`。
- 跨 workspace resume 拒绝有测试。
- interrupted turn 不写入 `messages.jsonl` 有测试。
- 配置优先级测试覆盖 default、user、workspace、env、CLI args。

退出条件:

- 修改 core/storage 时,主要兼容性问题能被测试捕捉。

## 7.2 0.5.2 Tool 与 Permission 测试加固

目标:把文件修改、命令执行和权限边界锁住。

交付:

- tool registry tests。
- path guard tests。
- command risk tests。
- permission mode tests。
- artifact truncation tests。

验收标准:

- `Read/Edit/Write/Glob/Grep/ListFiles/Bash` 都有成功和失败测试。
- 旧工具名兼容测试继续保留。
- workspace 外路径、路径穿越、symlink 越界都被拒绝。
- destructive command classification 有测试。
- `confirm`、`yolo`、`human` 行为有测试。
- 长 stdout/stderr 写 artifact 有测试。

退出条件:

- 工具协议变更不能静默破坏权限和 workspace 边界。

## 7.3 0.5.3 Agent 与 TUI 测试加固

目标:把端到端 loop 和 TUI 关键状态纳入 CI。

交付:

- deterministic mock provider scenarios。
- Agent loop integration tests。
- TUI snapshot tests。
- fixture workspace smoke tests。
- CI workflow 或本地等价脚本。

验收标准:

- mock provider 覆盖成功、unknown tool、tool failure、max_turns、cancel。
- 小型 Rust fixture 修复测试稳定通过。
- TUI snapshot 覆盖空状态、streaming、approval、tool output、error。
- CI 不要求真实 DeepSeek API key。
- 一条本地命令能跑完整内部测试门槛。

退出条件:

- 0.6 公开 benchmark 接入前,内部退化检查已稳定。

## 8. 0.6 Terminal-Bench Smoke

目标:接入主公开基准的小样本,验证真实终端任务的端到端能力。

范围:

- 增加 eval adapter。
- 跑固定 Terminal-Bench smoke subset。
- 保存每个任务的运行记录。
- 生成基础报告。
- 做失败原因粗分类。

交付:

- `eval` crate 或 `flash eval` 模块。
- `flash eval terminal-bench --subset smoke`。
- Terminal-Bench smoke adapter。
- 独立 eval run 目录。
- 每个任务的 session/events/artifacts。
- 结果 JSON。
- Markdown 简报。
- 失败原因分类。

验收标准:

- 能跑固定 Terminal-Bench smoke subset。
- 每个任务调用同一个 Agent runtime。
- 每个任务保存 `events.jsonl`。
- 每个任务保存最终 outcome。
- report 包含 task id、pass/fail、耗时、命令数、token usage、失败原因。
- 失败任务可 replay。
- 不存在评测专用 prompt/runtime 分支绕过正常 Agent。
- benchmark 环境错误和 agent 失败能区分。
- 多次运行能保存到不同 eval run 目录,不覆盖历史结果。

退出条件:

- 可以用 Terminal-Bench smoke 发现真实能力短板。
- 公开基准接入没有污染日常 CLI/TUI runtime。

## 8.1 0.6.1 Eval Harness 基础

目标:先建立通用评测运行框架,Terminal-Bench 只是第一个 adapter。

交付:

- `eval` crate 或 `flash eval` 模块。
- `EvalTask` / `EvalRun` / `EvalResult` 最小类型。
- eval workspace 隔离目录。
- timeout、retry、artifact 收集。
- JSON result writer。
- Markdown report writer。

验收标准:

- eval harness 可以运行一个本地 fixture task。
- eval task 调用同一个 `AgentRuntime`。
- eval run 输出目录不覆盖历史结果。
- task stdout/stderr、events、artifacts 都能保留。
- agent failure、environment failure、grader failure 能区分。

退出条件:

- 新 benchmark adapter 可以复用同一 eval harness。

## 8.2 0.6.2 Terminal-Bench Adapter

目标:接入固定小样本 Terminal-Bench,作为主公开基准 smoke。

交付:

- `flash eval terminal-bench --subset smoke`。
- Terminal-Bench task loader。
- task environment runner。
- grader result parser。
- smoke subset lock file。
- failure classifier。

验收标准:

- 能跑固定 smoke subset。
- 每个 task 都产生 session 和 event log。
- report 包含 task id、pass/fail、耗时、命令数、token usage、失败原因。
- fixed subset 版本固定,公开基准更新不会静默改变 smoke 口径。
- 不存在 Terminal-Bench 专用 runtime 或绕过工具权限的 prompt 分支。

退出条件:

- Terminal-Bench smoke 可以作为日常能力回归信号。

## 9. 0.6.x Skill 机制评估点

目标:只在真实需要时引入 Skill,避免过早把 prompt 系统复杂化。

这不是默认实现迭代,而是 0.6 后的评估门。

触发条件:

- 多个项目反复需要相同工程规范注入。
- Terminal-Bench/SWE-bench 失败分析证明缺少可复用策略层。
- 用户需要维护本地工作流说明。

验收标准:

- Skill 只能影响 prompt projection 或可用工具选择。
- Skill 不能绕过 permission policy。
- Skill 不能直接写 `messages.jsonl` / `events.jsonl`。
- Skill 文件中不得保存 secret 明文。
- Skill 行为必须能从 session replay 中解释。

退出条件:

- 如果没有明确收益,继续不实现动态 Skill。
- 如果实现,只作为 prompt/tool selection 扩展点,不成为新 runtime。

## 9.1 0.6.x.1 最小 Skill 方案候选

仅当触发条件满足时进入实现。候选方案保持克制:

- skill 是本地 Markdown/TOML 描述,不包含可执行代码。
- skill 只能提供 prompt 片段、工具使用偏好、项目规则。
- skill 来源必须记录到 `events.jsonl`。
- skill 不拥有独立权限模型。
- skill 不读写 session 文件。

验收标准:

- 未启用 skill 时行为完全不变。
- 启用 skill 后 replay 能看到使用了哪些 skill。
- skill 不能绕过 `PermissionPolicy`。
- skill 不能注入 secret 到日志。

## 10. 0.7 SWE-bench Verified Smoke

目标:验证真实开源 issue 的定位、修改和测试能力。

范围:

- 接入 SWE-bench Verified 小样本。
- checkout 目标 repo。
- 构造 issue prompt。
- 采集 patch。
- 对接 grader。
- 复用同一个 Agent runtime。

交付:

- `flash eval swe-bench --subset verified --limit 10`。
- SWE-bench Verified smoke runner。
- repo checkout 管理。
- issue prompt builder。
- patch collector。
- grader bridge。
- report JSON/Markdown。
- 失败分类。

验收标准:

- 能跑 10 个固定 SWE-bench Verified task。
- 每个 task 生成 patch 或明确失败原因。
- report 记录 resolved/unresolved。
- 失败可归类为定位失败、patch 失败、测试失败、环境失败、超时。
- 所有任务仍通过同一个 Agent runtime。
- patch 能映射到对应 session 和 events。
- grader 输出和 Agent event log 都能保留。
- 运行失败不会污染用户当前 workspace。

退出条件:

- 可以基于 SWE-bench smoke 分析真实代码修改短板。
- 评测结果能和 Terminal-Bench 结果并列比较,而不是只看单一总分。

## 10.1 0.7.1 SWE-bench Harness

目标:先跑通 checkout、patch、grader 数据链路。

交付:

- task metadata loader。
- repo checkout/cache。
- issue prompt builder。
- patch collector。
- grader command bridge。
- per-task cleanup。

验收标准:

- 能对 1 个固定 verified task 完整执行 checkout、agent、patch、grader。
- 运行目录与用户当前 workspace 隔离。
- patch、events、grader log 都能保留。
- task 超时后能清理进程并记录 timeout。

退出条件:

- SWE-bench 数据链路稳定,可以扩大到 smoke subset。

## 10.2 0.7.2 SWE-bench Verified Smoke

目标:把样本扩大到固定 10 个 task,并形成可比较报告。

交付:

- `flash eval swe-bench --subset verified --limit 10`。
- fixed task list。
- resolved/unresolved report。
- failure classifier。

验收标准:

- 10 个固定 task 能顺序运行。
- 每个 task 都能映射到 session id 和 patch。
- report 记录 resolved/unresolved、耗时、命令数、token usage、失败原因。
- 环境失败不会计为 agent resolved/unresolved。
- 不存在 SWE-bench 专用 runtime 或权限绕过。

退出条件:

- SWE-bench smoke 可以和 Terminal-Bench smoke 并列用于能力分析。

## 11. 0.7.x SubAgent / Task / AskUser 评估点

目标:只有单 Agent loop、基础 event 状态和 approval 已经不足时,再评估更复杂的交互协议。

这不是默认实现迭代,而是 0.7 后的评估门。

触发条件:

- 大仓库代码定位明显拖慢主 loop。
- 评测失败主要来自上下文收集不足。
- 需要把分析任务和执行任务隔离。
- TUI 中长期任务需要结构化 todo,单纯 events 难以表达进度。
- 模型频繁需要向用户询问歧义,approval 不能表达问题类型。

候选范围:

- `Agent`:只读 SubAgent,用于独立分析。
- `TaskCreate` / `TaskUpdate` / `TaskGet` / `TaskList`:结构化 todo 和进度。
- `AskUserQuestion`:结构化向用户提问。

验收标准:

- SubAgent 默认只读。
- SubAgent 不能直接执行写操作。
- SubAgent 结果作为 parent Agent 的普通上下文输入。
- SubAgent 事件可从 parent session 的 `events.jsonl` replay。
- SubAgent 不引入独立 session 存储格式,除非 replay 已无法表达。
- `Task*` 只能更新 session 内任务状态,不能替代 `messages.jsonl` 或 `events.jsonl`。
- `Task*` 状态必须能从 `events.jsonl` replay 重建。
- `AskUserQuestion` 在 TUI、CLI、headless eval 下都有明确行为。
- headless eval 下 `AskUserQuestion` 默认失败或使用预置答案,不能挂起无限等待。
- 所有新增工具仍受 permission policy 和 workspace guard 约束。

退出条件:

- 如果单 Agent loop 和基础 TUI 状态能解决主要问题,继续不实现这些工具。
- 如果实现,按 `Task*` -> `AskUserQuestion` -> 只读 `Agent` 的顺序评估,不做多 Agent 调度平台。

## 11.1 0.7.x.1 Task 工具候选

目标:让长任务进度可结构化展示,但不扩大存储模型。

交付:

- `TaskCreate`。
- `TaskUpdate`。
- `TaskGet`。
- `TaskList`。
- task state event。
- TUI task panel。

验收标准:

- task 状态可从 `events.jsonl` 完整重建。
- task 不单独落盘为新必需文件。
- replay 能展示 task 变化。
- task 工具不能执行文件或命令操作。
- headless CLI 中 task 变化能以简洁文本输出。

## 11.2 0.7.x.2 AskUserQuestion 候选

目标:只在 approval 不足以表达歧义时,允许模型提出结构化问题。

交付:

- `AskUserQuestion` tool spec。
- TUI 问题渲染。
- CLI 问题渲染。
- headless eval 默认策略。
- answer event。

验收标准:

- 问题必须包含明确选项或自由文本 schema。
- TUI/CLI 回答都写入 event。
- headless eval 没有预置答案时不会无限等待。
- 用户回答进入下一轮 model context。
- secret 类问题默认拒绝写入 events。

## 11.3 0.7.x.3 只读 Agent 候选

目标:只在评测证明需要时,引入可审计的只读 SubAgent。

交付:

- `Agent` tool spec。
- child context builder。
- parent event attribution。
- read-only tool registry。
- child summary result。

验收标准:

- child agent 默认只能使用 `Read`、`Glob`、`Grep`、`ListFiles`。
- child agent 不能调用 `Edit`、`Write`、`Bash`。
- child agent 输出只作为 parent 的 tool result。
- child events 可通过 parent session replay。
- child agent 有独立 max_turns 和 timeout。

## 12. 0.8 回归评测与报告

目标:形成小而稳定的防退化机制,用于 release 前检查。

范围:

- 固定内部 regression task set。
- 固定 Terminal-Bench subset。
- 固定 SWE-bench Verified subset。
- 汇总趋势报告。
- 对新增失败做高亮。

交付:

- `flash eval regression`。
- 私有 regression task set。
- Terminal-Bench fixed subset。
- SWE-bench Verified fixed subset。
- 趋势 JSON。
- Markdown 报告。
- replay 链接。

验收标准:

- 一条命令能跑 smoke regression。
- 报告能对比本次与上次 pass rate。
- 报告能列出新增失败任务。
- 报告能链接到对应 replay 文件。
- 报告区分 agent 失败、环境失败、benchmark 失败。
- release 前必须有一次 smoke regression 结果。
- fixed subset 版本固定,公开基准更新时不能静默改变历史对比口径。

退出条件:

- 每次 release 都能通过同一套 smoke regression 做横向对比。
- 公开 benchmark 用于发现能力短板,而不是只追总分。

## 13. 1.0 日常可用版本

目标:成为可日常使用、可恢复、可评测的 DeepSeek-first TUI Coding Agent。

范围:

- TUI 作为主交互入口。
- CLI 支持自动化和评测。
- session 可恢复、可 replay。
- 工具权限可靠。
- DeepSeek provider 稳定。
- 内部测试和公开 smoke 基准稳定。

交付:

- `flash`。
- `flash tui`。
- `flash run "<task>"`。
- `flash resume <session_id>`。
- `flash replay <events.jsonl>`。
- `flash doctor`。
- `flash eval terminal-bench --subset smoke`。
- `flash eval swe-bench --subset verified --limit 10`。
- 完整用户文档。
- 完整开发文档。

验收标准:

- 用户可用 `flash` 完成常见代码修改任务。
- 用户可用 `flash run` 做单次自动化任务。
- 用户可用 `flash resume` 在同一 workspace 继续历史 session。
- 跨 workspace resume 被拒绝。
- 用户可用 `flash replay` 复盘失败任务。
- TUI 中能查看模型输出、reasoning、tool output、approval 和 error。
- 所有工具调用都有权限判断。
- `Destructive` 永远需要显式确认。
- session/messages/events 存储结构稳定且有测试。
- 内部测试稳定通过。
- Terminal-Bench smoke 有报告。
- SWE-bench Verified smoke 有报告。
- 所有失败都有可追溯 event log。

退出条件:

- 项目可以作为日常 Coding Agent 使用。
- 后续新能力必须通过真实需求驱动,不能破坏 v1 的克制边界。

## 14. 后置能力清单

以下能力不进入默认 v1 计划,只有满足触发条件才设计:

- `index.json`:session 数量导致扫描明显变慢时。
- `trajectory.jsonl`:公开评测需要比 `events.jsonl` 更细的审计字段时。
- `compactions.jsonl`:上下文超限成为常见问题时。
- 独立 `sandbox` crate:工具执行边界复杂后。
- 独立 `telemetry` crate:成本、趋势、报表复杂后。
- 动态 Skill:反复出现可复用策略注入需求后。
- SubAgent:单 Agent loop 在大仓库定位中明显不足后。
- 更多 Provider:DeepSeek 路径稳定后。
- Web UI:当前不规划。
