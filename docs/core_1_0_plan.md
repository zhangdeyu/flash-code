# Flash Code 1.0 Core Runtime 收口计划

## 1. 范围

本计划只覆盖 macOS 上的 Core Runtime 收口。以下工作不在本轮范围：

- TUI 交互体验优化。
- CI、安装包和发布渠道。
- 完整用户文档与开发文档。
- 真实任务 dogfood、Terminal-Bench 和 SWE-bench 扩大运行。
- Linux、Windows 和其他平台支持。

本轮分为四个可独立回滚的阶段。每个阶段必须：

1. 先增加能证明目标行为的自动化测试。
2. 完成实现和必要的设计文档更新。
3. 通过该阶段的专项验收。
4. 通过完整质量门禁。
5. 创建独立 Git 提交后，才能进入下一阶段。

完整质量门禁：

```bash
cargo fmt --all -- --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features
```

## 2. 阶段 1：Session Continue 契约

### 目标

实现真正的历史会话继续运行，不修改父 session 的 append-only 日志。每次 continue 创建一个带 `parent_session_id` 的新 session，并从完整祖先链重建模型 history。

命令：

```bash
flash continue <session_id> "<instruction>"
```

### 实现范围

- Session metadata 增加 `parent_session_id`。
- Storage 提供 parent session 创建和祖先 history 加载 API。
- AgentRuntime 增加 continue 入口，复用同一个模型/Tool loop。
- CLI 增加 `continue` 命令。
- continuation 不复制或继承历史 approval event。
- 拒绝跨 workspace、循环 ancestry 和仍为 `running` 的父 session。

### 验收标准

- CLI 能解析 `continue <session_id> <instruction>`，旧 `resume` 仍不存在。
- continue 创建新 session，父 session 文件完全不变。
- 子 session metadata 正确记录 `parent_session_id`。
- Provider 收到祖先完整 history 和新的 user instruction。
- 两级 continuation 能按根到叶顺序重建 history。
- 跨 workspace continuation 被拒绝。
- `running` 父 session 在尚未恢复前被拒绝。
- 重建后的 history 通过 ToolTurn invariant check。
- 所有专项测试和完整质量门禁通过。

### 提交

```text
feat(session): add explicit continuation sessions
```

## 3. 阶段 2：原子存储与崩溃恢复

### 目标

让进程崩溃或最后一次写入中断后，workspace 仍可加载、replay 和 continue，并避免 metadata 半写状态。

### 实现范围

- `session.json` 使用同目录临时文件、flush、sync 和原子 rename。
- JSONL append flush；finalize 路径执行必要的 sync。
- 加载 session 时检测 `messages.jsonl` / `events.jsonl` 的损坏尾行。
- 只允许修复最后一个不完整 JSONL record；中间损坏必须报错。
- 启动恢复将遗留 `running` session 终结为 `failed`，写入可追溯恢复事件。
- recovery/finalize 保持唯一 `SessionFinished`。
- 增加存储 fault fixture，不依赖真实断电。

### 验收标准

- metadata 写入失败不会留下空文件或半个 JSON object。
- 最后一行截断的 messages/events 可被确定性修复，完整前缀不变。
- 中间 record 损坏会报错，不会静默跳过。
- 遗留 `running` session 恢复后状态为 `failed`。
- 恢复产生唯一 `SessionFinished(Failed)` 和明确错误事件。
- 已终结 session 重复恢复是幂等操作。
- 恢复后的 session 可 replay，也可作为 continue 父 session。
- 所有专项测试和完整质量门禁通过。

### 提交

```text
feat(storage): add atomic writes and crash recovery
```

## 4. 阶段 3：DeepSeek 请求可靠性

### 目标

为真实 DeepSeek V4 流式请求建立确定的 timeout、重试和响应大小边界。真实任务 benchmark 不在本阶段，但协议兼容路径必须可自动验证。

### 实现范围

- 配置化 connect timeout、首包 timeout 和 stream idle timeout。
- 429/5xx 支持 `Retry-After`。
- Agent retry 使用指数退避和确定范围内的 jitter。
- 认证、计费、非法请求和已发布 delta 的失败不透明重试。
- 限制 HTTP error body、单个 SSE frame 和累计 tool arguments 大小。
- timeout 和 size limit 映射为明确 ProviderError，进入统一 session finalize。
- 使用本地 HTTP fixture 覆盖真实 reqwest byte stream，不调用付费 API。

### 验收标准

- connect/首包/idle timeout 都能在限定时间内失败并 finalize。
- 429 的 `Retry-After` 被解析并影响下一次 attempt。
- 5xx 按上限重试，退避不会形成忙循环。
- 401/402/400 不重试。
- 已持久化 delta 后发生错误不重试。
- 超大 error body、SSE frame 和 tool arguments 被拒绝且内存有界。
- UTF-8/SSE/ToolCall 既有测试继续通过。
- 所有专项测试和完整质量门禁通过。

### 提交

```text
feat(provider): harden DeepSeek timeout and retry behavior
```

## 5. 阶段 4：macOS Sandbox 与资源边界

### 目标

在明确只支持 macOS 的前提下，让 Bash 网络策略和进程控制由真实系统行为证明，并限制单次任务可消耗的磁盘与日志资源。

### 实现范围

- 启动时验证 `sandbox-exec` 可用；不可用时 `allow_network=false` 的 Bash 直接拒绝。
- macOS sandbox profile 真实阻断 socket 网络访问。
- 保持 timeout/cancel 终止整个进程组，并验证无残留后代进程。
- 增加单 artifact、单 session artifact 总量、单 event 和单 JSONL 文件上限。
- 超限返回结构化 Tool/Storage error，并进入统一 finalize。
- call id、artifact 文件和 symlink 继续按不可信输入处理。
- `flash doctor` 报告实际 sandbox 与资源策略。

### 验收标准

- macOS 下真实 socket 连接在 `allow_network=false` 时失败。
- `allow_network=true` 仍经过 permission policy。
- 缺少 `sandbox-exec` 时不会退化为静默执行。
- Bash timeout/cancel 后父进程和后代进程均不存在。
- 单 artifact 和 session 总量超限时停止写入并返回明确错误。
- event/JSONL 超限不会造成 session 假成功。
- workspace escape、symlink escape 和 artifact path 注入测试继续通过。
- `flash doctor` 显示 sandbox 可用性与所有资源上限。
- 所有专项测试和完整质量门禁通过。

### 提交

```text
feat(safety): enforce macOS sandbox and resource limits
```

## 6. 最终完成标准

四个阶段提交全部存在且顺序清晰；每个阶段的验收标准均有直接测试或命令输出证明；最终工作树不包含未提交的本轮改动；完整质量门禁在最后一个提交上再次通过。只有同时满足这些条件，本计划才算完成。
