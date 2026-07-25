# Flash Code 0.5.1 验收记录

版本:0.5.1 协议与存储测试加固

## 1. 验收命令

```bash
cargo test -p flash-core
cargo fmt --check
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --all-features
```

## 2. 验收标准映射

| 验收标准 | 结果 |
|---|---|
| 新增 event 字段必须兼容旧 golden 或显式更新 golden | `crates/core/tests/golden/events_v1.jsonl` 锁定 v1 event JSONL 顺序和字段;`flash-core::storage::tests::event_schema_should_match_golden` 覆盖全部当前 `Event` variant |
| session scan 不依赖 `index.json` | `flash-core::storage::tests::create_session_should_not_require_or_write_index_json` 覆盖 storage 不创建 index;既有 `flash-tui::app_state_load_should_scan_sessions_without_index` 覆盖 TUI 通过 `session.json` 扫描 |
| 跨 workspace resume 拒绝有测试 | `flash-core::storage::tests::load_session_should_reject_workspace_mismatch` 覆盖 storage 层拒绝;TUI 0.4.3 已覆盖错误展示 |
| interrupted turn 不写入 `messages.jsonl` 有测试 | `flash-core::storage::tests::streaming_events_should_not_commit_interrupted_assistant_message` 覆盖 streaming delta/error 只进 events,不提交 assistant message |
| 配置优先级测试覆盖 default、user、workspace、env、CLI args | `flash-core::config::tests::load_should_keep_defaults_when_no_sources_are_present` 和 `load_should_apply_full_precedence_for_model_and_approval_mode` 覆盖完整顺序 |

## 3. 新增测试与 Fixture

- `crates/core/tests/golden/events_v1.jsonl`
- `crates/core/tests/golden/messages_v1.jsonl`
- `flash-core::storage::tests::event_schema_should_match_golden`
- `flash-core::storage::tests::message_schema_should_match_golden`
- `flash-core::storage::tests::streaming_events_should_not_commit_interrupted_assistant_message`
- `flash-core::storage::tests::create_session_should_not_require_or_write_index_json`
- `flash-core::config::tests::load_should_apply_full_precedence_for_model_and_approval_mode`
- `flash-core::config::tests::load_should_keep_defaults_when_no_sources_are_present`

## 4. 结论

- 0.5.1 已把 replay/resume 最敏感的协议、消息、存储和配置边界纳入自动化测试。
- 下一阶段进入 0.5.2 Tool 与 Permission 测试加固。
