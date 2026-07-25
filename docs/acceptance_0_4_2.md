# Flash Code 0.4.2 验收记录

版本:0.4.2 TUI Session 与输入流

## 1. 验收命令

```bash
cargo fmt --check
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo build -p flash-cli
FLASH_TUI_ONCE=1 FLASH_TUI_TASK="list files" cargo run -p flash-cli --
FLASH_TUI_ONCE=1 FLASH_TUI_TASK="list files" FLASH_TUI_CANCEL_AFTER_START=1 cargo run -p flash-cli --
cargo run -p flash-cli -- run "list files"
cargo run -p flash-cli -- --help
rg -n '"role":"assistant"|run cancelled|session_finished' .flash/sessions/session_1784946345742900000 -g '*.jsonl'
```

## 2. 验收标准映射

| 验收标准 | 结果 |
|---|---|
| TUI 发起任务和 `flash run` 使用同一 `AgentRuntime` 入口 | `flash-cli::CliTaskRunner` 通过 `AgentRuntime::run_task_controlled` 执行 TUI 任务;`flash run` 继续使用同一 runtime |
| streaming text/reasoning 先写 event,再由 TUI 渲染 | `AgentRuntime::emit_event` 先 `append_event`,再调用 observer;TUI observer 收到 event 后立即 push transcript 并 render |
| cancel 后 session 状态为 canceled 或 failed,不会写入半截 assistant message | `FLASH_TUI_CANCEL_AFTER_START=1` smoke 显示 `Status: cancelled`;`rg` 检查取消 session 只有 `run cancelled` 和 `session_finished` event,没有 assistant message |
| tool result 在 TUI 和 replay 中展示一致 | TUI live transcript 渲染 `requested ListFiles`、`started ListFiles`、`stdout`、`finished success`;这些来源于同一 `events.jsonl` |
| TUI crash 或主动退出后,已写入 events 仍可 replay | Agent event 先落盘再通知 TUI;observer/render 失败不会发生在 event 落盘之前 |
| headless eval 不依赖 TUI 模块 | `flash-agent` 不依赖 `flash-tui`;TUI 通过 `TaskRunner` trait 调用外部 runtime |

## 3. 新增测试

- `flash-agent::run_task_with_observer_should_emit_live_events_after_storage_commit`
- `flash-agent::run_task_controlled_should_cancel_without_committing_assistant_message`
- `flash-tui::run_task_for_state_should_render_live_task_events_and_final_status`
- `flash-tui::run_task_for_state_should_render_cancelled_status`
- `flash-tui::start_task_should_focus_transcript_on_current_task`

## 4. 结论

- TUI 可以通过 input/run path 发起一个只读任务。
- TUI live transcript 展示 reasoning、assistant、tool lifecycle、usage 和最终状态。
- 取消路径已贯穿 TUI runner 到 Agent runtime。
- 0.4.3 继续补 approval 和 resume。
