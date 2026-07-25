# Flash Code 0.5.2 验收记录

版本:0.5.2 Tool 与 Permission 测试加固

## 1. 验收命令

```bash
cargo test -p flash-tools
cargo test -p flash-core
cargo test -p flash-agent run_task_should_write_large_tool_output_to_artifact
cargo fmt --check
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --all-features
```

## 2. 验收标准映射

| 验收标准 | 结果 |
|---|---|
| `Read/Edit/Write/Glob/Grep/ListFiles/Bash` 都有成功和失败测试 | `flash-tools` 覆盖 v1 工具成功与失败路径:read missing、edit missing find、write invalid input、glob/grep invalid root、list missing dir、bash failing command |
| 旧工具名兼容测试继续保留 | `builtin_registry_should_expose_new_protocol_names_and_legacy_aliases` 覆盖 `read_file`、`apply_patch`、`write_file`、`search`、`shell`、`git_diff`、`run_tests` |
| workspace 外路径、路径穿越、symlink 越界都被拒绝 | `read_file_should_reject_workspace_escape`、`write_file_should_reject_parent_dir_escape`、`write_file_should_reject_absolute_escape`、`write_file_should_reject_symlink_escape`、`edit_should_reject_parent_dir_escape` |
| destructive command classification 有测试 | `v1_tool_risks_should_match_protocol` 和 `bash_risk_should_classify_destructive_commands` 覆盖 `Bash("rm -rf ...") -> Destructive` |
| `confirm`、`yolo`、`human` 行为有测试 | `flash-core::tools::permission_policy_should_cover_confirm_yolo_and_human_modes` 覆盖全部 mode/risk 矩阵 |
| 长 stdout/stderr 写 artifact 有测试 | `flash-agent::run_task_should_write_large_tool_output_to_artifact` 覆盖 artifact 文件内容、event 摘要和 message 摘要 |

## 3. 新增测试

- `flash-core::tools::permission_policy_should_cover_confirm_yolo_and_human_modes`
- `flash-tools::v1_tool_risks_should_match_protocol`
- `flash-tools::read_should_read_file_contents`
- `flash-tools::read_should_report_missing_file`
- `flash-tools::list_files_should_report_missing_directory`
- `flash-tools::glob_should_report_invalid_workspace_root`
- `flash-tools::grep_should_report_invalid_workspace_root`
- `flash-tools::edit_should_report_find_text_missing`
- `flash-tools::edit_should_reject_parent_dir_escape`
- `flash-tools::write_should_create_file_contents`
- `flash-tools::write_should_report_invalid_input_shape`
- `flash-tools::bash_should_execute_successful_command`
- `flash-tools::bash_should_return_error_for_failing_command`
- `flash-tools::bash_risk_should_classify_destructive_commands`
- 强化 `flash-agent::run_task_should_write_large_tool_output_to_artifact`

## 4. 结论

- 0.5.2 已把工具协议、权限矩阵、workspace path guard 和长输出 artifact 纳入自动化测试。
- 下一阶段进入 0.5.3 Agent 与 TUI 测试加固。
