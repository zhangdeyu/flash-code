# Flash Code 0.2 验收记录

版本:0.2 DeepSeek Headless Agent MVP

## 1. 验收命令

已执行:

```bash
cargo fmt --check
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo run -p flash-cli -- run "list files"
cargo run -p flash-cli -- replay .flash/sessions/session_1784942226450080000/events.jsonl
```

结果:全部通过。

## 2. 验收标准映射

| 标准 | 证据 |
|---|---|
| mock DeepSeek streaming 覆盖 reasoning/text/tool/usage/done | `flash-deepseek::parse_sse_should_emit_reasoning_text_tool_usage_and_done` |
| DeepSeek adapter 转换 provider-agnostic `ProviderEvent` | `flash-deepseek::parse_sse_should_emit_reasoning_text_tool_usage_and_done` |
| `flash run "list files"` 能触发工具并结束 | CLI 输出 `outcome: succeeded` |
| tool call 成功/失败/拒绝/取消都有 result | `flash-agent` tests 覆盖 success、error、rejected、cancelled 四种 tool result |
| unknown tool 不 panic,写 error tool result | `flash-agent::run_task_should_write_error_tool_result_for_unknown_tool` |
| 每轮 loop 受 `max_turns` 限制 | `flash-agent::run_task_should_stop_at_max_turns` |
| v1 不执行 parallel tool calls | `AgentRuntime` 顺序处理 `tool_calls` |
| 模型流取消不写半截 assistant | `flash-agent::run_task_should_not_commit_partial_assistant_without_done` |
| shell timeout 写 error tool result | `flash-tools::shell_should_return_error_on_timeout` |
| replay 能显示模型输出、reasoning 和工具输出 | replay 输出包含 `reasoning_delta`、`assistant_delta`、`tool_output_delta` |
| 429/500/503 有有限重试 | `flash-agent::run_task_should_retry_retryable_provider_errors` 和 `ProviderError::is_retryable` |
| 401/402 不重试 | `flash-deepseek::map_error_should_not_retry_authentication_errors` |
| DeepSeek API key 不写入日志/messages/events | 0.2 只读取配置中的 env name,不读取或落盘 secret 值 |

## 3. 已知边界

- 0.2 的 `flash run` 默认使用本地 smoke provider 验证 agent loop;真实 DeepSeek 网络请求将在后续 provider HTTP 层补全。
- `confirm` 模式下无交互 headless CLI 会自动执行 Read 风险工具,对需要确认的工具写 rejected result。
- 长工具输出会写入 session `artifacts/`,event/message 保留截断摘要和 artifact 路径。
