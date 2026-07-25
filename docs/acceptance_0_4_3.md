# Flash Code 0.4.3 验收记录

版本:0.4.3 TUI Approval 与 Resume

## 1. 验收命令

```bash
cargo fmt --check
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo build -p flash-cli

# from /private/tmp/flash-code-0-4-3-fixture
# TUI approved code modification fixture
FLASH_TUI_ONCE=1 FLASH_TUI_TASK="fix failing tests" FLASH_TUI_APPROVE=1 /Users/derek/Project/github/zhangdeyu/flash-code/target/debug/flash
cargo test
git diff -- src/lib.rs
rg -n 'approval_required|approval_resolved|call_patch_1|call_tests_1|call_diff_1' .flash/sessions/session_1784946767078370000/events.jsonl
/Users/derek/Project/github/zhangdeyu/flash-code/target/debug/flash replay .flash/sessions/session_1784946767078370000/events.jsonl

# from /private/tmp/flash-code-0-4-3-human
# human mode fixture
FLASH_TUI_ONCE=1 FLASH_TUI_TASK="fix failing tests" FLASH_TUI_APPROVE=1 /Users/derek/Project/github/zhangdeyu/flash-code/target/debug/flash
sed -n '1,20p' src/lib.rs
rg -n 'tool_started|approval_resolved|tool_finished' .flash/sessions/session_1784946808000264000/events.jsonl

# same workspace: /private/tmp/flash-code-0-4-3-fixture
# resume
FLASH_TUI_ONCE=1 FLASH_TUI_RESUME=session_1784946767078370000 /Users/derek/Project/github/zhangdeyu/flash-code/target/debug/flash

# cross workspace mismatch: /private/tmp/flash-code-0-4-3-resume-other
FLASH_TUI_ONCE=1 FLASH_TUI_RESUME=session_1784946767078370000 /Users/derek/Project/github/zhangdeyu/flash-code/target/debug/flash
```

## 2. 验收标准映射

| 验收标准 | 结果 |
|---|---|
| `Edit`、`Write`、`Bash` 需要确认时,TUI 能渲染待审批项 | TUI approval panel 显示 pending tool、risk 和 y/n 操作;smoke 中 `Edit`、`Bash cargo test`、`Bash git diff` 都产生 approval 事件 |
| 审批结果写入 `approval_resolved` event | `session_1784946767078370000/events.jsonl` 中 `call_patch_1`、`call_tests_1`、`call_diff_1` 都有 `approved:true` |
| destructive 命令在任何模式下都必须显式确认 | `flash-agent::run_task_should_require_approval_for_destructive_tool_even_in_yolo` 覆盖 yolo + destructive 仍产生 `approval_required` |
| `human` 模式下 TUI 只展示建议操作,不执行写入或命令 | human fixture 显示 `Permission: Human`;文件仍为 `41`;event log 只有 `approval_resolved:false` 和 rejected tool result,没有 `tool_started` |
| TUI session 列表通过扫描 `.flash/sessions/*/session.json` 生成,不依赖 `index.json` | `flash-tui::app_state_load_should_scan_sessions_without_index` 覆盖;TUI smoke 显示 session list |
| 在 B workspace resume A workspace session 会拒绝并展示明确错误 | resume-other smoke 首屏展示 `session belongs to ... current workspace is ...` |
| TUI 中完成一次小型 Rust fixture 修复,最终测试通过且 diff 可见 | `/private/tmp/flash-code-0-4-3-fixture` 中 TUI approved task 修复 `41 -> 42`;`cargo test` 通过;`git diff -- src/lib.rs` 可见 |

## 3. 新增测试

- `flash-agent::run_task_with_controls_should_execute_approved_tool_call`
- `flash-agent::run_task_should_require_approval_for_destructive_tool_even_in_yolo`
- `flash-tui::resume_session_should_load_transcript_for_current_workspace`
- `flash-tui::resume_session_should_render_workspace_mismatch_error`
- `flash-tui::run_task_for_state_should_render_pending_approval_and_approved_status`

## 4. 结论

- TUI 已支持 approval panel、approve/reject、permission mode 展示和 resume。
- TUI 与 CLI 共享 Agent runtime、权限策略和 session/event 存储。
- 0.4 TUI MVP 的 0.4.1/0.4.2/0.4.3 分段目标已完成。
- 下一阶段进入 0.5 内部自动化测试体系。
