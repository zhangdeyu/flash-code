# Flash Code 0.3 验收记录

版本:0.3 代码修改能力

## 1. 验收命令

已执行:

```bash
cargo fmt --check
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo build -p flash-cli

# fixture: /private/tmp/flash-code-0-3-fixture
cargo test
/Users/derek/Project/github/zhangdeyu/flash-code/target/debug/flash run "fix failing tests"
cargo test
git diff -- src/lib.rs
/Users/derek/Project/github/zhangdeyu/flash-code/target/debug/flash replay .flash/sessions/session_1784942565535377000/events.jsonl
```

结果:全部通过。fixture 初始测试失败,运行 `flash run "fix failing tests"` 后测试通过。

## 2. 验收标准映射

| 标准 | 证据 |
|---|---|
| `flash run "fix failing tests"` 能读文件、改文件、跑测试 | fixture replay 包含 `read_file`、`apply_patch`、`run_tests` 对应 tool events |
| 所有文件写入限制在 workspace root 内 | `writable_workspace_path` / `existing_workspace_path` 强制 canonical root 校验 |
| 路径穿越、symlink 越界、绝对路径越界拒绝 | `flash-tools` tests 覆盖 parent dir、symlink、absolute escape |
| destructive 命令即使在 `yolo` 下也必须确认 | `PermissionPolicy` 对 `Destructive` 永远返回 `Ask` |
| `human` 模式下不会真实执行 shell/write/test | `PermissionPolicy::Human` 对所有工具返回 `Deny`,agent 写 rejected result |
| 修改后输出最终 diff 摘要 | fixture `git diff -- src/lib.rs` 显示 `41 -> 42`; replay 包含 `git_diff` tool output |
| 测试失败时保留 stderr/stdout 摘要 | `flash-tools::run_tests_should_preserve_failure_output` |
| 测试输出过长写入 artifact | `flash-agent::run_task_should_write_large_tool_output_to_artifact` |
| apply patch 失败保留失败原因 | `flash-tools::apply_patch_should_report_find_text_missing` |
| 同一任务失败时可 replay | `flash replay` 输出完整 tool loop 时间线 |
| 简单 prompt projection/token budget | `flash-agent::project_history_should_keep_recent_messages_within_budget` |

## 3. Fixture 结果

修复前:

```text
answer_should_be_42 ... FAILED
left: 41
right: 42
```

修复后:

```text
answer_should_be_42 ... ok
test result: ok. 1 passed
```

diff:

```diff
-    41
+    42
```

## 4. 已知边界

- `apply_patch` v0.3 使用最小 find/replace 格式,不支持完整 unified diff。
- `git_diff` 依赖 workspace 是 git repo;非 git repo 会返回工具错误并写入 tool result。
- 复杂 compaction 仍后置,当前只做简单 tail projection。
